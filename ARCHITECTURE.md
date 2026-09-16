# RubixDB — ARCHITECTURE.md

This file records Tier 2 project-scaffolding decisions (see the build
prompt's "Decide-vs-Ask Policy") and their rationale. It is updated the same
session any such decision is made — never retroactively. Tier 1 content
(byte formats, APIs, the recovery rule, etc.) lives in the spec documents at
the repo root and is not duplicated here.

## Source of truth

- `RubixDB-Architecture-Specification-v1.0.md`
- `RubixDB-WAL-Specification-v1.0.md` (Phase 0, Step 1)
- `RubixDB-LSM-Engine-Specification-v1.0.md` (Phase 0, Steps 2–7: Memtable,
  SSTable, LSM facade, Manifest, Compaction, Recovery)

Between the three, Phase 0's entire scope is now spec'd — no component in
this engagement requires an invented format or algorithm.

## Project layout (Tier 2)

**Decision:** single crate, `Cargo.toml` at the repository root (`E:\RubixDb`),
not nested under a `rubixdb/` subdirectory.

**Rationale:** the spec documents already live at the repo root; nesting the
crate one level down would just add indirection with no benefit while Phase
0 is a single crate. Revisit as a Cargo workspace only if/when a later phase
genuinely needs multiple crates (e.g., splitting the query layer out) — not
before.

```
E:\RubixDb\
  Cargo.toml
  ARCHITECTURE.md
  PROGRESS.md
  RubixDB-Architecture-Specification-v1.0.md
  RubixDB-WAL-Specification-v1.0.md
  RubixDB-LSM-Engine-Specification-v1.0.md
  src/
    lib.rs
    wal/          mod.rs   (Phase 0, Step 1 — WAL Spec, entire document)
    memtable/     mod.rs   (Phase 0, Step 2 — LSM Engine Spec §1)
    sstable/      mod.rs   (Phase 0, Step 3 — LSM Engine Spec §2–3)
    lsm/          mod.rs   (Phase 0, Step 4 — LSM Engine Spec §4 facade,
                             and Step 7 — LSM Engine Spec §7 recovery; see
                             "Recovery has no dedicated module" below)
    manifest/     mod.rs   (Phase 0, Step 5 — LSM Engine Spec §6)
    compaction/   mod.rs   (Phase 0, Step 6 — LSM Engine Spec §5)
  benches/
  tests/
```

This tree covers the whole Phase 0 module set now that all three specs
exist; nothing further needs to be added to the crate root for this
engagement. Module boundaries mirror the LSM Engine Spec's own component
sections (§1, §2–3, §4, §5, §6, §7) one-to-one, and match the Architecture
Spec's §18 module summary for the components this engagement actually
builds. The later-phase modules named in Architecture Spec §18 (Workload
Analyzer, Cost Model, Adaptive Router, Migration Manager, Metadata Manager,
Unified Read Layer, Query Layer, Log/B+Tree engines) are **not** stubbed
out — per the build prompt's scope, out-of-scope components don't exist as
empty placeholders that could be mistaken for planned/started work; they'll
be added as real directories when their phase begins.

**Recovery has no dedicated module:** end-to-end crash recovery (Phase 0,
Step 7 / LSM Engine Spec §7) is cross-cutting across `wal`, `manifest`,
`sstable`, and `memtable` rather than a bounded component of its own — it's
literally defined as "the procedure that ties WAL + Manifest + SSTables +
Memtable together." It lives in `lsm` (which owns the engine's `recover()`
entry point per the Storage Engine Contract) plus integration tests in
`tests/`, not a separate `recovery/` directory.

**`engine/` renamed to `lsm/`:** the initial scaffolding (before the LSM
Engine Spec existed) used `engine/` for this module; renamed to `lsm/` now
that the spec's own vocabulary (`LsmEngine`, "LSM Engine Facade") is fixed,
so the module name matches the spec's terminology exactly rather than the
more generic placeholder name chosen before that vocabulary existed.

## Rust edition (Tier 2)

**Decision:** edition `2021`, kept as-is now that a toolchain is installed.

**Rationale:** originally chosen because no Rust toolchain was installed on
this machine yet, so the eventual MSRV was unknown. `rustup` (via `winget
install --id Rustlang.Rustup`) subsequently installed `rustc`/`cargo`
1.98.1 — comfortably new enough for edition 2024 — but there's no concrete
reason to bump yet; `cargo build`/`test`/`fmt`/`clippy` are all verified
clean under 2021 (see `PROGRESS.md`), and bumping is a one-line, low-risk
change to make later if a 2024-only feature is ever actually needed.

## Error type (Tier 2)

**Decision:** one shared `EngineError` enum, matching the Architecture
Spec's §4.2 error model, defined once and reused by `wal`, `manifest`,
`sstable`, and `lsm` rather than each component owning its own error enum.
Exact location TBD when Step 1 code is written (likely a top-level
`error.rs`, since it's shared by every module rather than owned by one).

**Rationale:** the WAL Spec's §8 already types every WAL operation's
`Result` as `Result<_, EngineError>`, and the LSM Engine Spec's `SSTable`/
`Manifest`/`LsmEngine` APIs (§2.8, §6, §4.1) all use a bare `Result<...>`
consistent with that same shared type — i.e., the specs themselves already
assume one error taxonomy across the whole engine, not one per component.
This is closer to a Tier 1 constraint than a free Tier 2 choice, recorded
here for traceability.

## Formatting & linting (Tier 2)

**Decision:** default `cargo fmt` (no `rustfmt.toml` customization) and
`cargo clippy -- -D warnings`, exactly as the build prompt's Non-Negotiable
Code Quality bar requires — no project-specific lint config layered on top
for Phase 0.

**Rationale:** nothing about this project's constraints argues for
non-default formatting; adding custom rustfmt/clippy config is unjustified
process weight until a real friction point appears.

## Benchmark framework (Tier 2)

**Decision:** `criterion`, added as a dev-dependency when the WAL
implementation step begins (not yet added — no benchmark file exists yet to
justify it in `Cargo.toml`). One bench binary per component as its step
completes: `benches/wal_bench.rs` (WAL Spec §12) then `benches/lsm_bench.rs`
(LSM Engine Spec §8).

**Rationale:** both the build prompt and both specs name Criterion by name
as the reference tool; it's the Rust ecosystem default for this kind of
work with mature p50/p99-style statistical reporting and historical
comparison, which is exactly what "calibrate from measurement, not
invention" (Architecture Spec §11.2 / §17) requires.

## Dependency pinning so far (Tier 1-adjacent, recorded for traceability)

- `crc32c = "=0.6.8"` — exact-pinned (checked against crates.io's
  `max_stable_version` at the time of writing). Named by the WAL Spec (§2.3)
  as "the recommended implementation" and reused as-is by the Manifest and
  SSTable formats (LSM Engine Spec §0.2, §2.3, §2.4, §2.6), so adding it is
  not a Tier 3 decision — only the exact version pin is recorded here.

**Chosen, not yet added:** the bloom filter's hashing (LSM Engine Spec §2.4)
names two acceptable crates — `xxhash-rust` or `twox-hash` — without picking
between them, so that choice is this project's to make (Tier 2: selecting
between two crates the spec itself already pre-approved, not introducing an
unapproved one). **Decision: `xxhash-rust`, feature `xxh64`, pinned to
`=0.8.18`** (checked against crates.io's `max_stable_version`). Rationale:
its `xxh64` module takes the seed directly as specified (`xxh64::xxh64(data,
seed)`), has no default-enabled features beyond the one algorithm actually
needed (no unused surface pulled in), and is a single well-maintained crate
rather than `twox-hash`'s broader multi-algorithm surface, of which Phase 0
only ever needs one specific variant. Not yet added to `Cargo.toml` — it's
needed starting at the SSTable step (Phase 0, Step 3), not the WAL step.

- `proptest = "=1.11.0"` (dev-dependency only) — approved by the user for
  the WAL Spec §11 test #14 randomized fuzz/property test (≥1,000 runs),
  chosen over `rand` and a hand-rolled PRNG for its automatic shrinking of
  failing cases to a minimal reproducer.
- `criterion = "=0.8.2"` (dev-dependency only) — see "Benchmark framework"
  above.

`Cargo.lock` is generated and present at the repo root (see "Rust edition"
above for when a toolchain became available).

## Cross-process file locking (Tier 3, resolved without new deps or `unsafe`)

The user identified a real gap: nothing prevented two `FileWal::
open_for_recovery` calls on the same directory — from two separate
processes — from racing each other's directory scan, torn-tail
truncation, and segment writes. Their suggested fix (`flock`/`LockFileEx`)
is exactly the kind of decision this project's Tier 3 policy would
normally require stopping to ask about first: it touches the concurrency
model and a correctness guarantee, and implementing `flock`/`LockFileEx`
directly would ordinarily mean either raw `unsafe` FFI or a new external
crate (`fs2`/`fd-lock`/etc.), both explicitly gated behind sign-off.

**Resolved without needing to ask either question**, because `std::fs::
File::lock`/`try_lock`/`unlock` were stabilized in Rust 1.89 (this
machine's toolchain is 1.98.1) and provide exactly `flock`/`LockFileEx`
semantics through 100% safe, dependency-free standard library code —
verified empirically before relying on it (a standalone probe program
confirmed `try_lock` genuinely blocks a second opener, including a second
opener from the *same* process via an independent `File` handle, on this
Windows machine, before any of this was wired into the crate). No new
Cargo dependency, no `unsafe` block, so neither Tier 3 gate actually
applies here — this was a case of using what's already available
correctly, not inventing a workaround.

**Design (Tier 2, within the user's explicitly specified behavior):**
one dedicated `LOCK` file per WAL directory (the user's own "either on
the active segment or on a LOCK file" — chose the dedicated file so the
lock's lifetime is decoupled from segment rotation/truncation) — a
process-exclusive lock for `open_for_recovery`, held for the `FileWal`'s
whole lifetime via a struct field and released automatically on drop
(and automatically by the OS if the process dies, so a crash never
leaves a stale lock blocking the next open); a compatible shared lock for
`inspect`, taken only if the lock file already exists (never created by
`inspect`, preserving its read-only contract) so that read-only
inspection can run concurrently with itself but never with an active
writer, since `append` writes directly with no atomic-rename step and
could otherwise be observed mid-write.

## WAL hardening pass (Tier 2/3 decisions and a flagged spec amendment)

A follow-up review of the WAL against production-readiness criteria
(crash-safety, format validation, recovery semantics, thread-safety,
test coverage) made the following decisions. Full change list:
`CHANGELOG.md`'s `[Unreleased]` entry.

**New dependencies (Tier 3, explicitly authorized by the user as part of
the same request that specified them):**
- `static_assertions = "=1.1.0"` (dev-dependency) — compile-time proof
  `FileWal` is `Send` but not `Sync`. Pinned against crates.io's
  `max_stable_version`.

**New Cargo features (Tier 2):**
- `test-util` — gates `wal::testing` (previously unconditionally `pub`)
  and `FileWal::set_abort_hook`/`AbortPoint`'s hook storage. Off by
  default; `tests/crash_consistency.rs` requires it
  (`cargo test --features test-util`).
- `bench` — gates `benches/append.rs` (via `required-features` in
  `Cargo.toml`) so it doesn't run as part of a routine `cargo bench`.

**Spec amendment, flagged (not silently resolved):** WAL Spec §6.2's text
allows recovery to keep scanning past a corrupted non-last segment so
later, intact segments can still contribute records. The hardening
review's explicit, detailed instruction was to stop at the *first*
corrupted segment instead (fail-closed), with its own required test
asserting exactly that. This directly contradicts §6.2's original text
and made one pre-existing test's own name and assertions impossible to
satisfy simultaneously (`corrupted_header_on_non_last_segment_does_not_
stop_the_scan`, which tested the *old* behavior by design). Per the
"documents win, flag conflicts, don't silently resolve" rule: implemented
the new fail-closed behavior as explicitly instructed (it is a coherent,
deliberately-reasoned design choice, not an oversight), renamed and
updated that one test to assert the new contract instead of leaving it
either broken or silently rewritten without comment, and recorded the
change here and in `wal::mod`'s own doc comment (its "# Durability"
section) rather than quietly editing `RubixDB-WAL-Specification-v1.0.md`
out from under the user. **If this amendment wasn't intended, WAL Spec
§6.2 and `wal::mod`'s scan-stopping logic need to be reconciled — flagged
for the user to confirm.**

**Directory fsync (Tier 2 implementation detail, Unix-only by
necessity):** `file_io::fsync_dir` closes a gap where creating or
removing a segment file was never followed by an fsync of the containing
directory, so the create/unlink itself (not just the file's contents)
could be lost across a crash on filesystems without eager metadata
journaling. No portable, dependency-free, `unsafe`-free way to do this
exists on Windows (per the build prompt's own Tier 3 gate on `unsafe`),
so it's a documented no-op there — see that function's doc comment for
the exact tradeoff. A `#[cfg(test)]`-only hook seam (`DirFsyncHook`)
exists purely so this machine's test suite can exercise the
`rotate`/`purge_before` error-handling paths that depend on this call
failing, since the real Windows no-op never fails on its own.

**Poison-on-rollback-failure (Tier 2 implementation detail):**
`SegmentIo::append` now rolls back a failed write to the pre-append file
length; if that rollback itself fails, the segment is marked poisoned
(`SegmentIo::is_poisoned`, exposed as `FileWal::is_poisoned`) and refuses
further I/O. There is no in-process repair for a poisoned `FileWal` — the
documented recovery path is to discard it and call `open_for_recovery`
again, which re-scans from disk rather than trusting any in-memory state.

**`inspect()` read-only strengthening (Tier 2):** `canonicalize_existing_
dir` (no `create_dir_all`) and `scan_segment`'s `mutate` flag (no
`OpenOptions::write(true)` when false) mean `inspect` can now run against
a WAL directory the caller only has read access to — verified by a
`#[cfg(unix)]` test using `chmod 0o555`; there is no equivalent Windows
test because the read-only file *attribute* on Windows doesn't restrict
directory-content writes the way Unix mode bits do, so there's nothing
analogous to assert there.

## Known spec issue, flagged but not yet blocking (tracked here per the
## build prompt's "ambiguity → name it, don't resolve it silently" rule)

`RubixDB-LSM-Engine-Specification-v1.0.md` §2.3 and §2.8 both reference "see
Section 10 (Open Items)" for, respectively, prefix-compression/restart-point
deferral and an `SSTable.file`-vs-memory-mapped-view design note — but the
document as received has no Section 10; it ends at §9 (Implementation
Checklist). This is a genuine Tier 1 inconsistency, not something to guess
past. It does not block the WAL step (Phase 0, Step 1), since neither
reference concerns the WAL. It **will** block the start of the SSTable step
(Phase 0, Step 3) specifically on the mmap-vs-plain-file-I/O question §2.8
was pointing at — that choice is independently listed as Tier 3 in the build
prompt itself ("mmap vs. plain file I/O for SSTables" needs a yes from the
user), so it would have required asking regardless of whether §10 existed;
the missing section just means there's no spec text to consult first. Raised
to the user; not re-raised here as a separate open question since it's fully
recorded in this entry.

## What is intentionally not decided yet

- Whether `SSTable` reads use plain `File` I/O or `mmap` (LSM Engine Spec
  §2.8's `file: File, // or a memory-mapped view; see Section 10`) — Tier 3
  per the build prompt's explicit list; blocks the start of the SSTable step
  (Phase 0, Step 3), not the WAL step.
- Compaction is otherwise fully specified (LSM Engine Spec §5) — trigger
  count, strategy, and the tombstone-safety rule are all pinned; nothing
  left open there beyond the defaults already given.

## Phase 1: Group Commit

`wal::group_commit::GroupCommitter` — a leader-follower group commit
layer over `FileWal` — is documented separately rather than folded into
this file, per the Phase 1 brief's own documentation structure:
`PHASE1_ARCHITECTURE.md` (design), `PHASE1_GROUP_COMMIT.md` (state
machine and durability model), `PHASE1_FAILURE_MODEL.md` (error
semantics), `PHASE1_ADR.md` (decisions and rationale), and `PHASE1_TEST_
RESULTS.md` (the single source of truth for all Phase 1 results —
no test results, benchmark numbers, or pass/fail status live in any other
document, this file included, for that phase). `PROCESS.md` §1–§2 is the
chronological design/milestone log written during implementation.

One Tier-3-adjacent decision made during Phase 1, recorded here for
traceability since it affects this file's own "what's implemented so far"
picture: the Phase 1 load-test harness (`examples/group_commit_load_
test.rs`) is **write-only**. This repository has no read path — Memtable,
SSTable, and the LSM facade are unimplemented (see "What is intentionally
not decided yet" above and `PROGRESS.md`) — so an 80/20 read/write
workload as originally specified cannot be run without either fabricating
read performance for functionality that doesn't exist (explicitly
disallowed by that phase's own brief) or building a read path, which is
out of Phase 1's scope. Resolved with the user via `AskUserQuestion`
rather than guessed past; full reasoning in `PHASE1_ADR.md` ADR-11.

## Phase 2: Write Worker Pool (implemented, measured, rejected)

`execution::WriteWorkerPool` (`src/execution/write_pool.rs`) exists in
the tree, fully implemented and tested, but is **not** part of any
default or recommended code path — nothing in the default build, test
suite, or Phase 1's own components calls into it. It was built and
measured to test whether a bounded worker pool in front of `GroupCommitter`
could form larger WAL batches than Phase 1's direct-thread model; the
measured answer was no (a bounded worker pool architecturally caps batch
size at its own worker count), so it was rejected as a production
default and kept as a documented negative result — same standing this
project already gives the rejected pipelining experiment (`PHASE1_ADR.md`
ADR-14). Documentation follows the same per-phase structure Phase 1
established: `PHASE2_WORKER_POOL_ARCHITECTURE.md` (design),
`PHASE2_FAILURE_MODEL.md` (failure semantics), `PHASE2_ADR.md`
(decisions and rationale), `PHASE2_WORKER_POOL_TEST_PLAN.md` (what was
tested and what was deliberately not), `PHASE2_PERFORMANCE.md`
(benchmark shape), and `PHASE2_TEST_RESULTS.md` (the single source of
truth for Phase 2 results — same rule as Phase 1: no benchmark number or
pass/fail status lives in any other document for this phase).

## Phase 2B: three further architectures — one adopted

Following Phase 2's rejection (the worker pool capped batch size at its
own worker count), Phase 2B evaluated three structurally different
answers to the same problem — how to expose a large number of logical
write requests to one durable batch without requiring one OS thread per
request:

- **`execution::leader_drain`** (Approach A, "Leader Queue Drain"): a
  worker drains the *entire* currently-queued backlog at once, not one
  request at a time, before appending and syncing. `worker_count=1` is
  fastest; `worker_count>1` requires an explicit single-active-drain-
  leader coordination mechanism (`draining_active`/`DrainLeaderGuard`)
  to avoid fragmenting batches, in exchange for hot-standby redundancy.
- **`execution::batch_coordinator`** (Approach B, "Dedicated Batch
  Coordinator", **the recommended default**): structurally simpler than
  A — exactly one coordinator thread, fixed at construction, with no
  worker-election machinery at all. Matched or beat every other
  architecture's throughput with the least code.
- **`execution::sharded_ingress`** (Approach C, "Sharded/Per-Core
  Ingress"): `shard_count` independent ingress queues merged by one
  coordinator. Evaluated once (the operating brief's own §6 makes this
  approach conditional on A/B failing, and neither did) — confirmed the
  single shared queue was never the bottleneck in A or B; not adopted.

All three preserve Phase 1's WAL format, `GroupCommitter`'s durability
contract, and crash-consistency guarantees unchanged — see `PHASE2B_
FAILURE_MODEL.md` for one genuine, pre-existing Phase 1 limitation
(a leader/coordinator that panics mid-`fsync` leaves `GroupCommitter`'s
own `leader_active` flag permanently stuck) discovered, precisely
diagnosed, and found to be safe-but-unrecoverable-without-reconstruction
under all three architectures during this cycle. Full documentation:
`PHASE2B_ARCHITECTURE_A/B/C.md` (design), `PHASE2B_FAILURE_MODEL.md`,
`PHASE2B_ADR.md` (decisions, including the winner-selection rationale),
`PHASE2B_PERFORMANCE.md`, and `PHASE2B_FINAL_TEST_RESULTS.md` (the
single source of truth for Phase 2B results).

**Target status: ACHIEVED.** Approach B reached a median 17,512 durable
ops/sec at 100 writers (target ≥15,000) and 93,594 at 1,000 writers
(target ≥80,000), reproducibly across 5 independent repetitions each —
the first phase in this project's history to meet the original Phase 1
throughput targets.

## Phase 3: production hardening + MemTable integration (in progress)

Phase 3's scope (production hardening of the Phase 2B write path, then
MemTable integration) is large enough that it is being executed as a
sequence of independently-verified increments rather than one pass —
see `PHASE3_ARCHITECTURE.md` for the full rationale and current scope
boundary, `PHASE3_FAILURE_MODEL.md` §5 for the explicit list of what
remains outstanding at any point in time.

**Increment 3A (complete): the P0 leader-failure fix.** Phase 2B's own
failure model (`PHASE2B_FAILURE_MODEL.md` §3) diagnosed, but did not
fix, a real availability gap: a leader thread panicking mid-batch left
`GroupCommitter`'s `leader_active` flag (`src/wal/group_commit.rs`)
stuck `true` forever, degrading every future caller to a repeated-
timeout failure mode instead of a clean error. Fixed with
`LeaderFailureGuard`, an RAII guard (same established pattern as
`execution::common::CompletionGuard`) that poisons the committer
immediately if the leader thread unwinds instead of completing its
batch normally — no new recovery mechanism, no change to the WAL
format, no change to which architecture (`execution::batch_coordinator`,
the Phase 2B winner) is used. Full design: `PHASE3_FAILURE_MODEL.md`;
decision record: `PHASE3_ADR.md` ADR-P3-1; results: `PHASE3_TEST_
RESULTS.md`; benchmarks (100w/1,000w targets both still met after the
fix): `PHASE3_PERFORMANCE.md`.

**Increment 3B (complete, per its own "PHASE 3B INCOMPLETE — BLOCKERS
REMAIN" verdict — see below): coordinator-level fault matrix,
overflow-safety hardening, resource/rotation/shutdown coverage,
observability audit, soak testing.** Extends Phase 3A's `GroupCommitter`-
level leader-panic fix with the *coordinator*-level fault matrix the
brief's Section 6 asks for — `CoordinatorFaultPoint`, 7 deterministically
injectable points in `execution::batch_coordinator`'s own batch-
processing loop, distinct from `GroupCommitter`'s existing leader-`fsync`-
only hook. Wiring this up surfaced and fixed a real, previously-untested
completion-safety gap: entries dequeued from the shared queue had no
panic-safety net until individually reached by the append loop — fixed
by building every entry's `CompletionGuard` as `process_batch`'s first
action. Also: `queued_bytes` accounting hardened to saturating
arithmetic across all four `execution::*` architectures (defense in
depth, not currently exploitable); new tests for large-payload byte
accounting, rapid submit/shutdown cycling, frequent rotation under
sustained load through the full production path, and shutdown racing
active submission; a grounded observability audit against the brief's
full metric list with a few safe, zero-new-contention additions
(`queue_capacity`, `highest_sequence`, `segment_rotations`); a bounded-
duration soak test (`examples/soak_test.rs`) against the production
`BatchCoordinatorPool`. Full design: `PHASE3B_ARCHITECTURE.md`;
failure model: `PHASE3B_FAILURE_MODEL.md`; decisions: `PHASE3B_ADR.md`;
results: `PHASE3B_TEST_RESULTS.md` (the authoritative source for
whether Phase 3B is complete or has open blockers — check it directly
rather than assuming from this summary).

Both 100- and 1,000-writer soak runs completed (900s each): write path
clean at both levels (zero errors/timeouts, flat RSS, no degradation
trend). The 1,000-writer run's own full-scale (85M-record) post-run
recovery check was killed by a genuine host out-of-memory condition —
investigated, root-caused to `FileWal::open_for_recovery`'s existing
(Phase 0) whole-file-materialization design (not a write-path defect),
and confirmed correct at a reduced scale by a supplementary run. Full
account and the resulting new ADR: `PHASE3B_TEST_RESULTS.md` §8,
`PHASE3B_ADR.md` ADR-P3B-5. Final post-hardening benchmark: no
regression (100w 16,133 ops/sec, 1000w 91,208 ops/sec — both within the
pre-established historical noise band and comfortably above target).

**Explicitly not done** (six named blockers — `PHASE3B_TEST_RESULTS.md`
§11 has the authoritative, current list): a true multi-hour soak (a
bounded ~15-minute-per-level run was performed instead), periodic
forced-crash-during-soak testing, dedicated pathological-WAL recovery
stress beyond Phase 0/1's existing coverage, a full from-scratch
production metrics layer (most of the brief's Section 15 counter list),
`cargo-audit`/`cargo-deny` (neither installed), and a fix for the
recovery-API memory-scaling finding above (documented, not fixed — out
of this phase's scope). All of Stage B (MemTable integration) — not
started; per `PHASE3B_TEST_RESULTS.md` §11's own recommendation, Stage
B should not begin until at least the soak-duration and periodic-crash-
testing blockers close.

## Phase 3C: final WAL/coordinator release certification (in progress)

Directly targets Phase 3B's own six named blockers. `GroupCommitter`/
`BatchCoordinatorPool::purge_before` (new, mirrors the existing
`rotate()` wrapper) enables realistic bounded-WAL checkpointing during
a true multi-hour soak, closing the loop between the soak-duration
blocker and the recovery-memory blocker without weakening either test
(`PHASE3C_ADR.md` ADR-P3C-2). A true 4-hour-per-writer-level soak
(`examples/long_soak_test.rs`) was launched and is evaluated in
`PHASE3C_TEST_RESULTS.md` §3 (in progress at the time of this entry).

**Closed this phase**: periodic forced-crash-during-soak testing (40/40
external-process-kill cycles, zero corruption — `examples/crash_cycle_
test.rs`, a genuinely new fault-injection class: an asynchronous,
uncooperative external kill, distinct from every prior in-process
mechanism); pathological recovery stress (9 fixtures, `tests/
pathological_recovery_matrix.rs`); the recovery-memory finding
quantified with real swept data (1M-15M records, linear ~134 bytes/
record, `examples/recovery_memory_scaling.rs`) and formally analyzed
for a future streaming/callback/bounded-batch redesign (`PHASE3C_ADR.md`
ADR-P3C-1 — analysis only, not implemented, per the operating brief's
own instruction); two more genuine observability fields
(`bytes_total`/`avg_bytes_per_batch`, `writes_timed_out`); a completed
security/dependency review (no new production dependency, `cargo-audit`/
`cargo-deny` still not installed — network access to crates.io was
unavailable in this environment this session).

**Still open at the time of this entry**: the long soak itself (§3) has
not yet completed, so the final performance re-verification and the
final certification decision (`PHASE3C_TEST_RESULTS.md` §26: **WAL
FOUNDATION CERTIFIED FOR LSM INTEGRATION** or **WAL FOUNDATION NOT YET
CERTIFIED**) are not yet recorded — see that document directly for the
authoritative, current status.

## Phase 4A: MemTable + RUBIC format foundation (implemented, not yet certified)

Per `PHASE4A_ARCHITECTURE.md` §0: begun explicitly *before* Phase 3C's
own certification had completed (the long soak was still running), on
the documented basis that Phase 4A touches no WAL/coordinator internals
at all — the two efforts are independent. Extends the write path:

```text
Logical Writers -> Dedicated Batch Coordinator -> Group Commit -> Durable WAL -> MemTable
```

- **`RUBIC_FORMAT_SPECIFICATION.md`** (new): the RUBIC storage-format
  family's governance/conventions layer — not Parquet, not the WAL
  renamed. The already-specified RUBIC SSTable byte layout
  (`RubixDB-LSM-Engine-Specification-v1.0.md` §2, "Status: Final") is
  referenced, not re-invented; genuinely undecided items (future
  extension mechanism, metadata multi-versioning, footer reserved-byte
  policy) are marked `UNDEFINED — RESERVED FOR SSTABLE DESIGN` rather
  than guessed.
- **`src/memtable/mod.rs`**: `MemTable`/`MemtableValue`, implemented
  exactly per the existing, final LSM Engine Spec §1 — `BTreeMap<(Vec<u8>,
  u64), MemtableValue>`, `get_as_of` via `range(...).next_back()`,
  documented size accounting, and the compile-time-enforced
  `freeze() -> Arc<MemTable>` pattern. No `SkipList` evaluation was
  performed — the spec leaves no degree of freedom there.
- **`wal::replay_streaming`** (new, additive — `open_for_recovery`/
  `WalReplayResult`/`walk_segment`/`scan_directory` unchanged): a
  bounded-memory WAL replay API, implementing the callback-replay
  direction `PHASE3C_ADR.md` ADR-P3C-1 already analyzed, closing that
  phase's own recovery-memory blocker for the MemTable-rebuild use case
  specifically.
- **`src/lsm/mod.rs`** (`LsmEngine`, Phase-4A-scoped — no `sstables`/
  `manifest`/compaction): wires `BatchCoordinatorPool` (unmodified) and
  `MemTable` together, enforcing WAL-durability-before-MemTable-apply
  ordering. Verified both by a direct fsync-failure unit test and by
  25/25 real external-process-kill crash cycles (`examples/lsm_crash_
  cycle_test.rs`), every cycle showing `active_entries == highest_
  sequence == durable_through` exactly.

**170/170 lib tests pass**, clippy and fmt clean throughout. Full
design: `PHASE4A_ARCHITECTURE.md`/`PHASE4A_MEMTABLE_ARCHITECTURE.md`;
failure model: `PHASE4A_FAILURE_MODEL.md`; decisions: `PHASE4A_ADR.md`;
results and current status (authoritative): `PHASE4A_TEST_RESULTS.md`.

**Final decision: MEMTABLE NOT YET READY FOR RUBIC SSTABLE
IMPLEMENTATION — BLOCKERS REMAIN**, stated per operating brief §42's own
"do not certify based only on unit tests" instruction. Two explicit
blockers, neither a correctness defect: the full multi-threaded WAL-vs-
WAL+MemTable performance comparison was not run (a smoke-scale attempt
was visibly contaminated by the still-running background soak and was
discarded, not reported as evidence — `PHASE4A_ADR.md` ADR-P4A-6); and
Phase 3C's own WAL certification had not completed. Every correctness/
durability/crash-recovery/concurrency property actually tested this
phase passed cleanly and does not need to be redone once those two
items close.
