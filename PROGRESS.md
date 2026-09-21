# RubixDB — PROGRESS.md

Append-only, dated log. Never rewrite history here — add new entries at the
bottom.

---

## 2026-09-14

**Implemented:** No implementation code yet. Completed the pre-implementation
scaffolding step:

- Authored `RubixDB-Architecture-Specification-v1.0.md` and
  `RubixDB-WAL-Specification-v1.0.md` at the repo root (binding, Tier 1,
  per the user's own drafts).
- Created the Phase 0 project skeleton: `Cargo.toml` (package metadata +
  `crc32c = "=0.6.8"` dependency, no other dependencies yet), `src/lib.rs`,
  and empty `mod.rs` placeholders (doc comments only, no implementation) for
  `wal`, `memtable`, `sstable`, `engine`, `compaction`, plus empty
  `benches/` and `tests/` directories.
- Wrote the initial `ARCHITECTURE.md` recording Tier 2 scaffolding decisions
  (project layout, edition, error-type placement, fmt/clippy policy,
  benchmark framework choice).

**Tests passing:** None yet — no implementation exists.

**Explicitly not done yet:** WAL implementation (Phase 0, Step 1) itself has
not been started. No code in `src/wal/mod.rs` beyond a doc comment.

**Open Tier 3 question currently blocking further work:** which
crate/approach to use for the WAL Spec §11 test #14 (≥1,000-run randomized
fuzz/property test) — hand-rolled PRNG with no new dependency, the `rand`
crate for generation only, or the `proptest` crate for generation with
automatic input shrinking. Asked the user directly; implementation of Step 1
(WAL) will not begin until this is answered, per the build prompt's
communication protocol ("do not keep working on unrelated Tier 1/Tier 2 work
in the meantime if it would need to be redone depending on the answer" —
this specific question only affects test code, so unrelated Tier 1 WAL
*implementation* work, once approved to start, is not blocked by it; only
the fuzz test itself is).

**Note on toolchain:** no local Rust toolchain (`cargo`/`rustc`) is
installed on this machine. This doesn't block writing spec-conformant code,
but `cargo fmt`/`clippy`/`test`/bench execution and `Cargo.lock` generation
are not possible here until one is installed.

---

## 2026-09-14 (continued)

**Implemented:** Still no implementation code. `RubixDB-LSM-Engine-Specification-v1.0.md`
(Phase 0, Steps 2–7: Memtable, SSTable, LSM facade, Manifest, Compaction,
Recovery) was authored, completing the Phase 0 spec set — every component
in scope now has an exact specification. Updated scaffolding to match:

- Renamed `src/engine/` to `src/lsm/`, matching the spec's own vocabulary
  (`LsmEngine`), and added `src/manifest/`. Full module tree is now `wal`,
  `memtable`, `sstable`, `lsm`, `manifest`, `compaction` — one per LSM
  Engine Spec component section.
- Updated `src/lib.rs` and every `mod.rs` doc comment to point at the
  correct spec section for its component.
- Updated `ARCHITECTURE.md`: full Phase 0 tree, the `engine/`→`lsm/` rename
  rationale, the shared-`EngineError`-across-all-components rationale (now
  that the LSM Engine Spec's own APIs confirm this, not just the WAL Spec),
  and the `xxhash-rust` vs. `twox-hash` Tier 2 choice (decided:
  `xxhash-rust`, feature `xxh64`, pinned `=0.8.18` — not yet added to
  `Cargo.toml`, needed starting at the SSTable step, not the WAL step).
- Flagged a genuine Tier 1 inconsistency in the LSM Engine Spec: §2.3 and
  §2.8 both reference "Section 10 (Open Items)," which doesn't exist in the
  document as received. Recorded in `ARCHITECTURE.md`; does not block the
  WAL step; will block the start of the SSTable step (Phase 0, Step 3) on
  the mmap-vs-plain-file-I/O question specifically, which is Tier 3
  regardless per the build prompt's own explicit list.

**Tests passing:** None yet — no implementation exists.

**Explicitly not done yet:** WAL implementation (Phase 0, Step 1) itself has
still not been started.

**Open Tier 3 question, now resolved:** user approved `proptest` (dev-
dependency only) for the WAL Spec §11 test #14 fuzz/property test. Pinned
`proptest = "=1.11.0"` and `criterion = "=0.8.2"` (both dev-dependencies) in
`Cargo.toml`. No remaining Tier 3 blockers for the WAL step — beginning
implementation next.

---

## 2026-09-14 (WAL implementation begins)

**Implemented:** Starting Phase 0, Step 1 (WAL), per
`RubixDB-WAL-Specification-v1.0.md` in full.

**Tests passing:** None yet — implementation in progress this session.

**Explicitly not done yet:** everything in the WAL implementation checklist
(spec §13). This entry will be followed by further entries as pieces land.

**Open Tier 3 question currently blocking further work:** none identified
so far in the WAL spec itself; will flag immediately if one surfaces while
writing the code.

---

## 2026-09-14 (WAL implementation: first full pass, unverified)

**Implemented:** A complete, spec-conformant first pass of the WAL (Phase 0,
Step 1), covering every item in WAL Spec §13's implementation checklist:

- `src/error.rs`: shared `EngineError` enum (Architecture Spec §4.2) and
  `Result<T>` alias, used by every WAL module.
- `src/wal/format.rs`: 24-byte segment header encode/decode (§2.2), the
  generic `length‖crc32c‖body` frame encoder (§2.3), and the length-prefix
  read/write helpers used everywhere a length-prefixed field appears (§2.6's
  pattern, applied uniformly).
- `src/wal/ops.rs`: `WalOp`/`WalOpOwned` and their `op_body` encoding for
  `PUT`/`DELETE`/`CHECKPOINT_MARKER` (§2.4), rejecting `ENGINE_SWITCH` as
  not-yet-valid (reserved for Phase 5).
- `src/wal/file_io.rs`: the `WalFile` trait (`Read + Write + Seek` plus
  `sync_all`/`set_len`/`len`), implemented for `std::fs::File`; `MemFile`,
  an in-memory `WalFile` backend used by unit tests and the fuzz harness so
  ≥1,000-iteration runs don't touch a real filesystem; `SegmentIo<F>`, the
  generic single-segment append/sync core, monomorphized per `F` (no `dyn
  Trait` on the hot path).
- `src/wal/recovery.rs`: `walk_segment`/`walk_full_segment`, implementing
  the §6.2/§6.3 torn-vs-corrupt classification exactly — including the two
  distinct tail-eligibility tests (header-only-remaining for an
  out-of-range `length`, vs. full-claimed-extent for a CRC mismatch or
  decode failure), which the spec's text draws a real distinction between
  and which is easy to collapse incorrectly into one rule.
- `src/wal/testing.rs`: `FaultInjectingIo<F>` (§11's required harness),
  injecting configurable write/`fsync` failures and short writes.
- `src/wal/mod.rs`: `WalConfig`/`SyncMode`/`WalPosition`/`WalReplayResult`,
  the `Wal` trait exactly as specified, `FileWal` (the production
  implementation: segment enumeration, canonicalized-and-verified paths
  per §8, rotation per §7 — including the "don't rotate an already-empty
  segment just to fit one oversized record" case §3.4 explicitly
  accommodates — and `purge_before` per §10's checkpoint-watermark safety
  rule), and `inspect()`.
- `src/wal/fuzz_tests.rs`: the §11 test #14 proptest harness (≥1,000 cases,
  `ProptestConfig::with_cases(1000)`), asserting the core invariant against
  randomized append sequences and randomized truncation points, entirely
  in-memory.
- `tests/wal_tests.rs`: integration tests against real files/directories
  for the checklist items that specifically need them (round trip,
  multi-segment replay, a real torn tail, real header corruption on a
  non-last segment, `MAX_RECORD_LEN` enforcement writing zero bytes,
  `purge_before` against real segment files, `inspect`'s byte-for-byte
  no-mutation guarantee, sequence resumption across a real reopen, the
  empty-WAL case).
- `benches/wal_bench.rs`: Criterion benchmarks for `append_sync` at a few
  payload sizes and recovery-replay throughput at a few record counts.
- Unit tests inline in `format.rs`, `ops.rs`, `recovery.rs`, `file_io.rs`,
  and `testing.rs` cover the byte-level round trips and the remaining §11
  items (torn mid-header, torn mid-body, torn-exactly-at-boundary,
  non-tail corruption never silently truncated, the fsync-failure/
  durability-watermark test) at the unit level, where the private
  `walk_segment`/`FaultInjectingIo` machinery is directly reachable.

**Tests passing: VERIFIED** as of this entry. `rustup`/`cargo`/`rustc`
1.98.1 were installed on this machine (via `winget install --id
Rustlang.Rustup`) specifically to close out the "unverified" gap the first
half of this entry originally recorded. Once installed:

- `cargo build`: clean, zero warnings.
- `cargo test`: **44/44 passing** — 35 unit tests (`format`, `ops`,
  `file_io`, `recovery`, `testing`, `wal::tests`, plus the §11 test #14
  proptest fuzz harness, which itself ran 1,000 cases) + 9 integration
  tests in `tests/wal_tests.rs`.
- `cargo fmt --check`: clean (after one `cargo fmt` pass — pure
  whitespace/wrapping, no logic changes).
- `cargo clippy --all-targets -- -D warnings`: clean, after fixing real
  findings caught along the way (not suppressed):
  - `WalFile::len` renamed to `size` (clippy's `len_without_is_empty` — a
    fallible file-size getter isn't the collection-`len()` shape that lint
    expects an `is_empty` companion for, so renaming was more honest than
    adding a token `is_empty`).
  - `ok_or_else(|| ...)` → `ok_or(...)` in `format::encode_frame` (the
    closure did no lazy-expensive work).
  - `criterion::black_box` → `std::hint::black_box` (deprecated API).
  - Two integration-test nitpicks: `.len() > 0` → `!.is_empty()`, and a
    `field_reassign_with_default` cleaned up into struct-update syntax.
- `cargo bench` (`wal_bench.rs`, release profile, default Criterion
  sampling): real numbers, not asserted from first principles —

  | Benchmark | Payload/count | Time (Criterion [low, mid, high] of the
    confidence interval) | Throughput |
  |---|---|---|---|
  | `append_sync` | 16 B | [6.83, 8.38, 10.18] ms | ~1.9 KiB/s |
  | `append_sync` | 256 B | [3.30, 3.40, 3.53] ms | ~73 KiB/s |
  | `append_sync` | 4096 B | [3.92, 4.46, 5.49] ms | ~896 KiB/s |
  | `recovery_replay` | 100 records | [1.07, 1.14, 1.23] ms | ~87 Kelem/s |
  | `recovery_replay` | 1,000 records | [7.48, 7.58, 7.70] ms | ~132
    Kelem/s |
  | `recovery_replay` | 10,000 records | [71.3, 74.2, 77.5] ms | ~135
    Kelem/s |

  Millisecond-scale `append_sync` latency is the expected shape for
  `Immediate`-mode `fsync`-per-call durability on this machine's disk, not
  a red flag — it's exactly the number the Architecture Spec's cost model
  (§11.2) will eventually calibrate `WriteCost` against once the router
  phase begins. Some outlier samples were reported by Criterion (noted in
  the raw run, not reproduced here); not investigated further since no
  correctness claim rests on them, only a rough throughput baseline.
- `Cargo.lock` generated and present at the repo root.

**Explicitly not done yet:** everything past Phase 0 Step 1 — Memtable
onward (LSM Engine Spec §1+).

**Open Tier 3 question currently blocking further work:** none. The next
step (Memtable) is fully specified by the LSM Engine Spec §1 and will get
its own restated-scope-and-Tier-3-question pass, per the build prompt's
per-step check, before any code is written for it.

---

## 2026-09-14 (WAL hardening pass — production-readiness review)

**Implemented:** A targeted 8-group hardening pass over the WAL
(`src/wal/`), requested directly by the user with per-fix required
behavior and required tests already specified. Full detail in
`CHANGELOG.md`'s `[Unreleased]` entry and `ARCHITECTURE.md`'s new "WAL
hardening pass" section; summary:

- Group 1 (crash-safety): `SegmentIo::append` rollback-on-partial-write-
  failure with poisoning if rollback itself fails; directory fsync after
  segment create/remove; atomic `create_new_segment_file` and `rotate`;
  one-at-a-time `purge_before` with consistent partial-failure state.
- Group 2 (format validation): `decode_segment_header` rejects unknown
  `format_version`/non-zero `flags`; `read_u32_le`/`read_u64_le` return
  `Result` instead of relying on a release-mode-inert `debug_assert!`;
  frame encoding is fully fallible end-to-end, no more sentinel-value
  workaround.
- Group 3 (recovery semantics): scans now stop at the first corrupted
  segment — **a deliberate amendment to WAL Spec §6.2's original "keep
  scanning past a corrupt non-last segment" text**, explicitly flagged,
  not silently applied (see below); `walk_segment`'s one `.expect()` on
  a not-quite-provably-unreachable case replaced with a proper `Err`;
  segment-ID arithmetic checked against overflow.
- Group 4: `inspect()` never creates the WAL directory and never opens a
  segment file for writing, so it works against a read-only directory.
- Group 5: `FileWal` documented and compile-time-asserted `Send + !Sync`;
  `SyncMode::GroupCommit` now rejected outright rather than silently
  downgraded to `Immediate`; `wal::testing` gated behind a new
  `test-util` Cargo feature.
- Group 6: `SegmentIo::append` uses a new `WalFile::write_all_at`
  (`pwrite` on Unix, portable fallback elsewhere) instead of a separate
  seek; added `benches/append.rs` behind a new `bench` feature.
- Group 7: three new ≥1,000-run proptest cases (random single-byte
  corruption, partial-write-with-garbage-header, 10,000-iteration
  fixed-seed arbitrary-noise panic check); `Fault::PartialThenFail`
  added to the fault-injection harness; new
  `tests/crash_consistency.rs` (behind `test-util`) spawning this test
  binary as a real child process that opens a real `FileWal` and calls
  `std::process::abort()` at one of four configurable points — genuinely
  exercised end-to-end on this machine (all four abort points fire and
  the parent's post-recovery assertions hold); a `#[cfg(unix)]`
  read-only-directory test.
- Group 8: checked the whole tree for the mojibake the request
  described — found none (see flagged item below); added
  `scripts/check-encoding.sh` (a corrected version of the requested
  check); added "# Safety"/"# Durability" sections to `wal::mod`'s
  module doc comment; `CHANGELOG.md` created; `ARCHITECTURE.md` updated;
  an explicit byte-for-byte wire-format regression test added
  (`wire_format_is_byte_for_byte_unchanged`) confirming the on-disk
  format is unchanged by this pass.

**Tests passing: VERIFIED.**
- `cargo test`: 60 lib tests + 10 integration tests (`tests/wal_tests.rs`)
  + 0 from `tests/crash_consistency.rs` (correctly compiles to nothing
  without `test-util`), all passing.
- `cargo test --release`: same, all passing.
- `cargo test --features test-util`: 60 + 2 (`crash_consistency.rs`'s
  parent test spawning and verifying all 4 real child-process abort
  points) + 10, all passing.
- `cargo fmt --check`: clean.
- `cargo clippy --all-targets --all-features -- -D warnings`: clean,
  after fixing real findings along the way (`io::Error::other`,
  a type-complexity lint on the test-only `DirFsyncHook` storage).
- `scripts/check-encoding.sh`: clean (no mojibake found — see below).

**Two things flagged for the user, not silently resolved:**
1. **WAL Spec §6.2 amendment.** The hardening request's Group 3.1
   ("stop at the first corrupted segment") directly contradicts §6.2's
   existing text ("replay does not stop here if it is not the last
   segment... subsequent segments... are still processed"), and made
   one pre-existing test's own name/assertions
   (`corrupted_header_on_non_last_segment_does_not_stop_the_scan`)
   impossible to keep unchanged while also satisfying Group 3.1's own
   required test. Implemented the new fail-closed behavior as
   explicitly, specifically instructed; renamed and updated that one
   test to assert the new contract (added a second test covering the
   complementary case) rather than leaving a contradiction in place.
   **`RubixDB-WAL-Specification-v1.0.md` itself has not been edited** —
   only the code and this project's own docs — so the spec document and
   the implementation now disagree on this one point until the user
   confirms which should give.
2. **The mojibake check as literally specified doesn't work on this
   codebase.** `grep -rP '[\x80-\xff]' src/` matches every byte of any
   multi-byte UTF-8 character, not mojibake specifically — run as
   specified, it flags hundreds of lines of this project's own correct,
   deliberate `§`/`—`/`'` usage. Searched instead for the actual
   reported mojibake sequences (`┬¦`, `ŌĆö`, `ŌĆÖ`) and for UTF-8
   validity: found nothing wrong. `scripts/check-encoding.sh` implements
   a corrected version of the intent.

**Explicitly not done yet:** everything past Phase 0 Step 1 (Memtable
onward) — this entire entry was a hardening pass on the already-"done"
WAL step, not new Phase 0 progress.

**Open Tier 3 question currently blocking further work:** the WAL Spec
§6.2 amendment above needs the user's confirmation before Memtable
work begins, in case it changes anything about how "trust nothing after
a corrupted segment" should compose with the LSM engine's own recovery
procedure (LSM Engine Spec §7, which was written assuming the WAL's
original §6.2 semantics).

---

## 2026-09-14 (cross-process file locking)

**Implemented:** the user flagged a real, serious gap: nothing stopped
two `FileWal::open_for_recovery` calls on the same directory (from two
separate OS processes) from racing each other and silently corrupting
the WAL. Fixed via `std::fs::File::lock`/`try_lock` (stabilized Rust
1.89, this machine runs 1.98.1) — `flock` on Unix, `LockFileEx` on
Windows, both through safe standard-library code. **No new dependency,
no `unsafe`** — verified this was actually available and worked as
expected (blocking a second opener, including from a second independent
`File` handle within the same process) via a standalone probe program
compiled and run on this machine *before* relying on it, rather than
trusting memory of the API shape. See `ARCHITECTURE.md`'s new
"Cross-process file locking" section for why this sidestepped what would
normally have been a Tier 3 stop-and-ask (raw `unsafe` FFI vs. a new
locking crate).

- `open_for_recovery`: exclusive lock on a dedicated `LOCK` file in the
  WAL directory, held for the `FileWal`'s whole lifetime, released on
  drop (and automatically by the OS if the process dies).
- `inspect`: compatible shared lock, taken only if the lock file already
  exists (never created — preserves `inspect`'s read-only contract).
- Three new in-process regression tests (`wal::mod`'s test module) plus
  one genuine cross-process test
  (`tests/wal_tests.rs::cross_process_lock_prevents_concurrent_writers`,
  spawning this same test binary as a real second OS process, the same
  technique `tests/crash_consistency.rs` uses) — actually run on this
  machine: the second process is rejected while the first holds the
  directory open, and succeeds once it's dropped.

**Tests passing: VERIFIED.** `cargo test`: 63 lib + 12 integration
(up from 60 + 10), all passing. `cargo test --release`: same. `cargo
test --features test-util`: same + 2. `cargo fmt --check`: clean.
`cargo clippy --all-targets --all-features -- -D warnings`: clean, after
fixing one real finding along the way (`clippy::suspicious_open_options`
on the lock file's `OpenOptions` — added an explicit `.truncate(false)`,
since the lock file's content is never used and must not be rewritten on
every open).

**Explicitly not done yet:** everything past Phase 0 Step 1 (Memtable
onward) — unchanged from the prior entry.

**Open Tier 3 question currently blocking further work:** unchanged —
the WAL Spec §6.2 amendment from the prior entry still needs the user's
confirmation before Memtable work begins.

---

## 2026-09-14 (five follow-up fixes from external review)

**Implemented:** five targeted fixes from a follow-up review, each with
code the reviewer largely specified directly:

1. `purge_before` now attempts its directory fsync on the error path,
   not just on success, and folds a remove failure + fsync failure into
   one error naming both. Dropped the unconditional `eprintln!`.
2. Documented (comment only, no code change) why recovery's torn-tail
   truncation doesn't need a directory fsync: `set_len`/`sync_all`
   flush the file's own inode metadata; no directory entry changes.
3. Real Windows `write_all_at` via a `seek_write` retry loop, replacing
   the portable seek-then-write-all fallback that path was silently
   using before.
4. Expanded `Fault::PartialThenFail`'s doc comment with its exact
   `write_all`-interaction semantics.
5. A `NOTE` comment above `scan_directory`'s main loop guarding against
   a future "fix" that loosens the deliberate stop-at-first-corruption
   behavior.

**A genuine finding, not just following instructions:** implementing
item 3 and un-gating its `#[cfg(unix)]` test (as the reviewer expected,
believing `seek_write` behaves like `pwrite` here) **failed when
actually run on this Windows machine**. Investigated rather than
papering over it: Windows' `seek_write`, on an ordinary synchronous
(non-`FILE_FLAG_OVERLAPPED`) file handle, genuinely does advance the
file's read/write position to just past the written region — a real,
verifiable platform difference from POSIX `pwrite`, which never touches
it. The byte content itself lands correctly at the correct offset on
both platforms; only this position side-effect differs, and nothing in
this crate's production code depends on it (every read path here seeks
explicitly first). Split the test into an unconditional content-
correctness check and a `#[cfg(unix)]`-only cursor-preservation check,
and corrected `WalFile::write_all_at`'s doc comment, which — before this
— was asserting a cross-platform guarantee that was actually only ever
true on Unix.

**Tests passing: VERIFIED.** `cargo test`: 66 lib + 12 integration (up
from 63 + 12 — 2 new `purge_before` failure-path tests, and the
`write_all_at` test split into 2, net +1 visible on this platform since
the Unix-only half doesn't compile here). `cargo test --release`: same.
`cargo test --features test-util`: same + 2. `cargo fmt --check`: clean.
`cargo clippy --all-targets --all-features -- -D warnings`: clean.

**Explicitly not done yet:** everything past Phase 0 Step 1 (Memtable
onward) — unchanged.

**Open Tier 3 question currently blocking further work:** unchanged —
the WAL Spec §6.2 amendment still needs the user's confirmation before
Memtable work begins. (Item 3 above is a flagged *finding*, not a
blocking question — it's resolved in code; flagged here only because it
contradicted the reviewer's stated expectation and is worth knowing
about.)

---

## 2026-09-14 (security + performance audit, `wal_test.md`)

**Implemented:** a full security and performance audit of `src/wal/` on
request, with results written to `wal_test.md` (new file, repo root) —
full test suite (debug/release/`test-util`, all 80 tests with `test-util`
enabled), a line-by-line security check against every item in the
project's Non-Negotiable Security bar (`unsafe` usage, `.unwrap()`/
`.expect()` on untrusted data, checked arithmetic, path canonicalization,
payload-content logging, dependency pinning), and both Criterion
benchmark suites run to completion with real numbers recorded.

**One real finding, fixed:** an inconsistency in `recovery::walk_segment`
— one `offset + FRAME_HEADER_LEN as u64` computation used raw arithmetic
where the structurally identical `claimed_end` computation a few lines
below it already uses `checked_add`. Not an exploitable bug (`offset` is
already provably bounded by that same prior `checked_add`), but
inconsistent with the project's own stated "never raw arithmetic on a
corruption-adjacent value" rule and with the code's own established
pattern a few lines away — changed to `saturating_add` for consistency.
Full detail and rationale in `wal_test.md` §3.7.

**Tests passing: VERIFIED**, post-fix — `cargo test`: 78/78 (debug and
release). `cargo test --features test-util`: 80/80. `cargo clippy
--all-targets --all-features -- -D warnings`: clean. `cargo fmt --check`:
clean.

**Explicitly not done yet:** everything past Phase 0 Step 1 (Memtable
onward) — unchanged. Also explicitly not verifiable from this Windows-
only session (noted honestly in `wal_test.md` §3.8, not glossed over):
real (non-hooked) POSIX directory-fsync behavior, the `chmod`-based
read-only-directory test, and Unix `pwrite`'s cursor-preservation
property.

**Open Tier 3 question currently blocking further work:** unchanged —
the WAL Spec §6.2 amendment still needs the user's confirmation before
Memtable work begins.

---

## 2026-09-14 (Phase 1: Group Commit)

**Implemented:** `wal::group_commit::GroupCommitter`, a leader-follower
group commit layer on top of the existing, frozen `FileWal` — concurrent
callers share one `fsync` per batch instead of one per write, via a
monotone `durable_through` watermark. Full design in `PHASE1_
ARCHITECTURE.md`/`PHASE1_GROUP_COMMIT.md`/`PHASE1_ADR.md`; full results
in `PHASE1_TEST_RESULTS.md` (the single source of truth for this phase's
numbers, per the user's own explicit rule — not duplicated here).

Also implemented, per a mid-session scope expansion the user authorized
after two hard blockers were flagged and resolved via `AskUserQuestion`
(no read path exists to load-test 80/20 against; no profiler tooling is
set up on this platform): `GroupCommitter` backpressure
(`with_max_pending_waiters`), explicit `shutdown()`, a `stats()`
observability snapshot, `AbortPoint` expanded from 4 to 11 variants (7
new, all real reachable boundaries in the group-commit leader/rotation
paths), a write-only load-test harness (`examples/group_commit_load_
test.rs`), and a permanent append-path diagnostic (`examples/append_
only_benchmark.rs`).

**Tests passing:** 87 lib tests (up from 80), all pre-existing WAL
integration tests unchanged and green, plus the full `tests/group_
commit/` suite (M1.1, M1.4, M1.5, M1.6, `watermark_monotonicity` all
pass in every configuration; M1.2/M1.3's throughput assertions do not —
see below). `cargo clippy --all-targets --all-features -- -D warnings`
and `cargo fmt --check`: clean.

**Explicitly not done:** the M1.2 (≥15,000 ops/sec, 100 writers) and
M1.3 (≥80,000 ops/sec, 1,000 writers) throughput targets are not met on
this development machine — root-caused (not merely observed) to this
machine's real SATA SSD `fsync` latency (~2.8–3.0ms) interacting with the
algorithm's own 200µs latency-protecting window cap, evidenced by an
isolated append-path diagnostic (~138–141k ops/sec, 4–47x above either
target on its own) and a specific, falsifiable counterfactual. Full
analysis in `PHASE1_TEST_RESULTS.md` §14–§18. **Phase 1 production-
readiness decision: NOT PRODUCTION READY**, this one blocker aside —
every other gate (correctness, crash-consistency across 11 abort points,
security checklist, zero regression) is met.

**Open question for the user, not resolved unilaterally:** whether to
accept the current implementation as correct-but-disk-bound on this
hardware (re-verify on faster storage before considering Phase 1 done),
or to pursue a different implementation strategy despite the evidence
that the append path itself is not the bottleneck (`PHASE1_TEST_
RESULTS.md` §15's dominant-cost analysis). Not re-litigated here — see
that document.

---

## 2026-09-14 (Phase 1: window-size sweep resolves the original root-cause claim)

**Implemented:** a follow-up review correctly identified that the prior
entry's "hardware-bound" conclusion rested on an experiment that could
not actually establish it (raising `max_wait` past the `EMA/10` cap was
tested; the cap itself, via the EMA divisor, was never varied). Ran the
corrected experiment: a temporary, feature-gated (`phase1-window-
experiment`, off by default) sweep of both `max_wait` and the EMA
divisor independently, 5 configurations × 3 repetitions × both M1.2/M1.3
workload shapes (30 runs). Full data and interpretation: `PHASE1_TEST_
RESULTS.md` §9A.

**Finding:** throughput scales substantially with window size (~1.7–2.2x
from baseline to the best tested window), plateauing once the window
exceeds available writer demand. Fixed the production formula
accordingly (`WINDOW_EMA_DIVISOR` `10 → 1`, test/harness `max_wait`
`200µs → 5ms`) — then found and fixed a real regression the sweep itself
couldn't surface (single-writer latency, M1.1: 2.905ms → 5.761ms) with a
demand-adaptive probe before the leader commits to the full window.

**Tests passing:** 87 lib tests, unchanged. Full `group_commit` suite
re-verified after the fix: M1.1/M1.4/M1.5/M1.6/`watermark_monotonicity`
all pass; M1.2/M1.3 improved substantially (100 writers: ~67%→~79% of
target; 1,000 writers: ~46%→~81%) but still miss their thresholds.
`cargo clippy`/`cargo fmt --check` clean, including the experiment
feature.

**Explicitly not done:** M1.2/M1.3 still do not hit their throughput
targets on this development machine even after the fix — `fsync` latency
(~2.8–3.0ms on this SATA SSD) remains a genuine floor no window-size
tuning removes. **Phase 1 production-readiness decision unchanged: NOT
PRODUCTION READY**, now backed by a controlled experiment and a verified
fix rather than algebra alone. Full account: `PHASE1_TEST_RESULTS.md`
§9A/§9B/§15 (revised)/§19; decision record: `PHASE1_ADR.md` ADR-12.

## Phase 2: Write Worker Pool — implemented, measured, rejected

Built `execution::WriteWorkerPool` (`src/execution/write_pool.rs`): a
bounded queue in front of a fixed number of worker threads, each calling
the same, unmodified `GroupCommitter::append`/`await_durable` a Phase 1
direct caller already used — meant to test whether separating logical
client concurrency (1,000s of callers) from physical storage execution
concurrency (a small, controlled worker count) could form larger, more
efficient WAL batches than Phase 1's direct-thread model. `std`-only
(`Mutex`+`Condvar`, no new dependency), no unsafe code, no change to
`GroupCommitter`'s durability logic or the WAL format — see `PHASE2_
WORKER_POOL_ARCHITECTURE.md` for the full design.

**Critical experiment**: worker-count sweep (1/2/4/8/16/32/64, plus a
parity point at `worker_count = writer_count`) at both 100 and 1,000
logical writers, same session, same commit as the Phase 1 baseline it
was compared against. **Finding**: `avg_batch_records` tracks
`worker_count` almost exactly at every point tested (e.g. 1,000 writers,
worker_count=64 → avg batch 62.89; worker_count=1,000 → avg batch
442.48) — a `GroupCommitter` batch can only ever contain requests that
have already reached `append()`, so a bounded worker pool caps batch
formation at its own worker count regardless of how many logical writers
are queued behind it. Even at the pool's best-case configuration
(`worker_count = writer_count`, eliminating that ceiling entirely), 1,000-
writer throughput was still 28% below Phase 1's direct-thread number,
from the pool's own added queue/completion/allocation overhead. Full
data: `PHASE2_TEST_RESULTS.md` §7–§10; root-cause and decision record:
`PHASE2_ADR.md` ADR-P2-5.

**Decision: REJECT** the worker pool as a production default — Phase 1's
existing direct-thread architecture remains faster at every tested and
reasonably extrapolatable configuration. The code is kept in the tree
(correctness- and fault-injection-tested, zero interaction with any
existing Phase 1 code path) as a documented negative result, mirroring
`PHASE1_ADR.md` ADR-14's own precedent for the rejected pipelining
experiment — not deleted, and not silently forced into production
because it looked like the expected next step.

**Fixed during this cycle**: the first implementation retried nothing —
a single-attempt `append_durable` call could surface a spurious
`Timeout` under real concurrent load even though the write was never
lost. Fixed by retrying only `await_durable` (never `append` — zero
duplicate-record risk), mirroring the retry pattern Phase 1's own test
harness (`tests/group_commit/support.rs::await_durable_retrying_on_
timeout`) already established as correct. Full account: `PHASE2_TEST_
RESULTS.md` §13; decision record: `PHASE2_ADR.md` ADR-P2-4.

**Tests**: 96 lib tests (87 unchanged Phase 1 + 9 new, including two
fault-injection tests — an injected `fsync` failure and a genuine worker-
thread panic, both verified to propagate faithfully to the caller
without hanging or losing other queued requests). Full Phase 1
regression gate (`cargo test`/`--release`/`--features test-util`/
clippy/fmt) and crash-consistency suite re-verified with zero
regressions. Full account: `PHASE2_TEST_RESULTS.md` §4–§5.

## Phase 2B: three architectures evaluated — target achieved

Rejected Phase 2's worker pool showed batch size following worker
count; `PHASE2_ADR.md` ADR-P2-5 named a specific alternative — let the
current leader drain *many* already-queued requests before syncing,
instead of requiring one worker per in-flight request. Phase 2B tested
that alternative and two further, genuinely distinct architectures:

**Approach A** (`execution::leader_drain`, "Leader Queue Drain"): a
worker drains the entire currently-queued backlog at once. Attempt A1
(`worker_count=1`): 16,806 ops/sec median at 100 writers, 95,686 at
1,000 — **both exceed target on the first attempt**. Attempt A1's own
sweep found `worker_count>1` *fragments* throughput (concurrent drainers
split one large batch into several smaller ones — 45,555–65,800 ops/sec
across worker counts 2–64 at 1,000 writers, all worse than
`worker_count=1`'s 85,158+). Attempt A2 fixed this with a single-active-
drain-leader coordination flag (`draining_active`/`DrainLeaderGuard`,
RAII, panic-safe): `worker_count=2` recovered to 92,172 ops/sec at 1,000
writers (matching `worker_count=1`) while adding hot-standby redundancy,
at a small, honestly-recorded cost to 100-writer margin (median 14,652,
just under target). Attempt A3 not needed.

**Approach B** (`execution::batch_coordinator`, "Dedicated Batch
Coordinator", **the winner**): structurally simpler than A — exactly
one coordinator thread, no worker-election machinery, decided at
construction rather than negotiated at runtime. Matched or beat every
other architecture: 17,512 ops/sec median at 100 writers (best of any
architecture measured), 93,594 at 1,000 writers. No optimization
attempts needed beyond the baseline implementation.

**Approach C** (`execution::sharded_ingress`, "Sharded Ingress"):
per the operating brief's own conditional framing ("if A and B fail..."),
evaluated once since neither failed. `shard_count` independent ingress
queues merged by one coordinator — 15,234 ops/sec median at 100 writers,
96,033 at 1,000 — no material improvement over Approach B, confirming
the single shared queue was never the bottleneck. Not adopted.

**A real bug found and fixed during Approach A's development**: the
first implementation constructed each entry's panic-safety guard
(`CompletionGuard`) *after* the batch-wide `await_durable` call rather
than before, leaving every entry in a batch unprotected during the one
call most likely to observe a fault. A panic there hung the worker-panic
fault-injection test past a 60-second timeout; fixed by building every
guard before the shared call. Approaches B and C were written after
this fix and used the correct ordering from the start.

**Also discovered, not a bug introduced this cycle**: a leader/
coordinator that panics mid-`fsync` leaves `GroupCommitter`'s own
`leader_active` flag permanently stuck (pre-existing Phase 1 behavior,
`src/wal/group_commit.rs`) — meaning Approach A's worker redundancy
cannot rescue a request submitted *after* this specific failure, since
the underlying committer itself becomes globally wedged, not just the
one thread that died. Verified the system still fails safely (bounded,
no hang, no false acknowledgment) under this condition regardless.

**Winner: Approach B**, selected by the operating brief's own priority
order (correctness/durability/stability tied across all three;
throughput favors B outright at 100 writers; complexity — the final
tiebreaker — favors B decisively, being the simplest of the three).
Approach A at `worker_count=2` is documented as the recommended
alternative for deployments requiring hot-standby redundancy.

**Final acceptance**: re-verified from the final clean commit — full
regression gate, crash-consistency suite (4 total runs across this
cycle), and 5 independent benchmark repetitions per level for Approach
B, all reproducible. **100 writers: median 17,512 ops/sec (target
≥15,000, +16.7%). 1,000 writers: median 93,594 ops/sec (target ≥80,000,
+17.0%). TARGET ACHIEVED** — the first phase in this project's history
to meet the original Phase 1 throughput targets, with zero durability or
crash-consistency regressions. Full account: `PHASE2B_FINAL_TEST_
RESULTS.md`; decisions: `PHASE2B_ADR.md`.

---

## 2026-09-16 (Phase 3, Increment 3A: the P0 leader-failure fix)

**Implemented:** Phase 3's operating brief is large (production
hardening of the Phase 2B write path, then MemTable integration —
`PHASE3_ARCHITECTURE.md` has the full scope). Per this project's own
"focused commits, one verified thing at a time" discipline, it is being
executed as a sequence of increments rather than one pass; this entry
covers the first, the P0 item the brief itself flags ahead of
everything else (§5): a leader thread panicking mid-batch left
`GroupCommitter`'s `leader_active` flag (`src/wal/group_commit.rs`)
stuck `true` forever — a real, previously-diagnosed-but-unfixed gap
from Phase 2B (`PHASE2B_FAILURE_MODEL.md` §3).

Before any code change: froze the Phase 3 baseline per the brief's own
§3 requirement — recorded `git status`/`git rev-parse HEAD`/`git log`
(clean tree, commit `7c808eb`, the last Phase 2B commit) and re-ran the
winning Approach B benchmark at that exact commit: 100 writers median
17,918 ops/sec, 1,000 writers median 97,434 ops/sec (both exceed
target, consistent with `PHASE2B_FINAL_TEST_RESULTS.md`'s own historical
17,512/93,594 within this machine's documented noise band). Full
numbers: `PHASE3_PERFORMANCE.md` §2.

Fix: `LeaderFailureGuard` (`src/wal/group_commit.rs`), an RAII guard —
the same established pattern `execution::common::CompletionGuard`
already uses — armed the instant a caller is elected leader and
disarmed only once `run_as_leader` returns normally. If the leader
thread instead panics, the guard's `Drop` (running during the unwind)
clears `leader_active` and poisons the committer
(`PoisonReason::LeaderPanicked`, a new enum replacing `BatchState::
poisoned`'s previous bare `io::ErrorKind`), so every other caller —
already-waiting follower or a later request — fails fast and cleanly
instead of riding out a timeout against a leader that will never be
elected again. No new recovery mechanism: the documented path is still
"discard this `GroupCommitter`, call `FileWal::open_for_recovery` again,
construct a fresh one" — unchanged from Phase 1's own model, verified
end-to-end by a new test that does exactly this and confirms the
pre-panic durable record survives a real reopen. Full state-machine
design and the "why poison unconditionally" analysis: `PHASE3_FAILURE_
MODEL.md`; decision record: `PHASE3_ADR.md` ADR-P3-1.

Two pre-existing Phase 2B tests
(`execution::leader_drain::tests::one_worker_panicking_...`/`::a_
second_request_after_the_leader_panics_...`) had doc comments and
implicit timing assumptions describing the old, now-fixed behavior
(~5s shutdown cost, a second request only failing after its full retry
budget) — updated in place with new timing assertions (`< 1s`) that
lock in the fix rather than leaving stale documentation next to
passing-but-now-misleading tests.

**Tests passing: VERIFIED.** `cargo test --lib`: 117/117 (115
pre-existing + 2 new: `leader_panic_clears_leader_active_poisons_and_
recovers_cleanly_on_reopen`, `concurrent_followers_all_fail_fast_when_
the_leader_panics`). `cargo test --release --lib`: 117/117. `cargo test
--lib --features test-util`: 117/117. `cargo clippy --all-targets
--all-features -- -D warnings`: clean. `cargo fmt --check`: clean (two
unrelated pre-existing trailing-newline nits in `src/wal/mod.rs`/
`src/wal/recovery.rs` fixed as a drive-by, zero logic change). `cargo
test --release --test crash_consistency --features test-util`: 2/2.
`cargo test --release --test group_commit --features test-util`: 6/8 —
only the pre-existing, unrelated Phase 1 direct-thread M1.2/M1.3
throughput misses, unchanged and unaffected by this fix. Full table:
`PHASE3_TEST_RESULTS.md`.

Post-fix benchmark (Approach B, same methodology): 100 writers median
15,329 ops/sec (target ≥15,000, +2.2% — down from the pre-fix 17,918
but still passing; attributed to this machine's own documented
run-to-run variance, not the fix itself, since the fix's cost on the
non-panicking hot path is one `bool` write plus one `bool` check, not
plausibly a double-digit-percent effect — flagged honestly rather than
asserted away, see `PHASE3_PERFORMANCE.md` §3 for the full reasoning).
1,000 writers median 93,733 ops/sec (target ≥80,000, +17.2% — matching
Phase 2B's own historical number almost exactly). **Both Phase 2B
throughput targets remain met.**

**Explicitly not done yet:** the rest of Phase 3's Stage A (full
fault-injection point matrix beyond the one leader-panic scenario,
coordinator-lifecycle formalization, shutdown-determinism audit, soak
testing, repeated crash testing, resource-exhaustion testing, a
production metrics/logging layer) and all of Stage B (MemTable
integration, not started). Full itemized list: `PHASE3_FAILURE_MODEL.md`
§5.

**Open Tier 3 question currently blocking further work:** none — the
next increment (continuing Stage A hardening, or beginning Stage B) has
no unresolved Tier 3 question yet identified.

---

## 2026-09-16 (Phase 3, Increment 3B: coordinator fault matrix + resource/rotation/shutdown hardening)

**Implemented:** continuing Phase 3's own "small, independently-verified
increments" discipline (`PHASE3_ADR.md` ADR-P3-2), this entry covers
Increment 3B: completing the coordinator-level half of Phase 3's
production-hardening scope (the operating brief's Section 6, distinct
from Increment 3A's `GroupCommitter`-level leader-panic fix), plus
resource-exhaustion, rotation-stress, shutdown, and observability
coverage.

Froze the baseline at commit `68d70ea` (last Phase 3A commit, clean
tree) — 100w median 17,582 ops/sec, 1000w median 92,671 ops/sec, both
comfortably above target, zero failures/timeouts across 6 runs. Full
numbers: `PHASE3B_TEST_RESULTS.md` §2.

Added `CoordinatorFaultPoint` (`src/execution/batch_coordinator.rs`):
7 deterministically injectable points in the Dedicated Batch
Coordinator's own batch-processing loop (before batch formation, after
drain, after append, before/after awaiting durability, before
completion, during shutdown) — none of which `GroupCommitter`'s
existing leader-`fsync`-only fault hook can reach. Wiring up the
`AfterDrain` test **surfaced a real, previously-untested correctness
gap**: `process_batch` only gave a dequeued entry its `CompletionGuard`
once the append loop individually reached it — a coordinator panic
between dequeue and that point would have dropped every entry in the
batch with callers hanging forever (the shared queue's own panic-safety
fallback, `CoordinatorAliveGuard`, only covers entries still in the
queue, not ones already handed to a local batch). Fixed by building
every entry's guard as `process_batch`'s first action, before any other
work. 7 new tests (one per fault point) all pass, verifying no caller
hangs, no universal false success, the pool reaches a terminal state,
and the WAL remains recoverable at every point. Full design: `PHASE3B_
FAILURE_MODEL.md`; decision record: `PHASE3B_ADR.md` ADR-P3B-1.

Also fixed a smaller, related gap found during the same audit:
`queued_bytes += approx_bytes` (raw addition) was inconsistent with the
saturating-arithmetic admission check right beside it, across all four
`execution::*` architectures — not currently exploitable (the admission
check already bounds accepted totals well below `usize::MAX`) but
inconsistent with this project's own established "never raw arithmetic
on a corruption-adjacent value" precedent (`wal_test.md` §3.7). Fixed
consistently across `batch_coordinator`/`leader_drain`/`sharded_
ingress`/`write_pool`. `PHASE3B_ADR.md` ADR-P3B-2.

New tests: large-payload byte accounting (200 KiB payloads, a
deterministic atomic-counter barrier — not sleep timing — pins down
exactly one in-flight entry before measuring `queued_bytes`), rapid
submit/shutdown cycling (25 iterations), frequent automatic rotation
under sustained concurrent load through the *full production path*
(extends the pre-existing `tests/group_commit/rotation_mid_batch.rs`
M1.5 coverage, which only exercises `GroupCommitter` directly, not the
coordinator sitting in front of it), and `shutdown()` racing active
submission.

Observability: audited existing `GroupCommitStats`/
`BatchCoordinatorStats` against the operating brief's full metric list
(Section 15) — added the genuinely safe, zero-new-contention gaps
(`queue_capacity`, `queued_bytes_capacity`, `highest_sequence`,
`segment_rotations`); explicitly did **not** attempt a full from-scratch
metrics layer this increment (`PHASE3B_ADR.md` ADR-P3B-3) — recorded as
an open item, not silently marked done. `examples/soak_test.rs`'s own
latency-sampling design (per-thread local slots, periodic aggregation
by a dedicated sampler thread) stands as a validated low-contention
reference for that future work — the same shape Phase 1's own
`batch_timing` module already proved necessary at this project's scale.

Soak test: built `examples/soak_test.rs` against the production
`BatchCoordinatorPool`; ran 100 writers for 900s (15 minutes) — **not**
the brief's requested multi-hour duration, flagged explicitly rather
than hidden or extrapolated (`PHASE3B_ADR.md` ADR-P3B-4). Result: zero
errors, zero timeouts, zero backpressure rejections across 15,495,498
completed ops; `queue_depth` was `0` at every sample (the coordinator
never fell behind); RSS *decreased* slightly over the run (no leak
trend); throughput at the end was *higher* than at the start (no
degradation trend); post-shutdown recovery: exact record count, zero
corruption, gap-free sequences, ~73s recovery time for 15.5M records
(consistent with this project's own Phase 0 recovery-throughput
benchmark). The 1,000-writer soak run was still in progress at the time
this entry was written — see `PHASE3B_TEST_RESULTS.md` for its result
once complete, and for the final post-hardening benchmark comparison
and Section 29 completion decision.

**Tests passing: VERIFIED.** `cargo test --lib`: 128/128 (117
pre-existing + 11 new). `cargo test --release --lib`: 128/128. `cargo
test --lib --features test-util`: 128/128. `cargo clippy --all-targets
--all-features -- -D warnings`: clean. `cargo fmt --check`: clean.
Zero regressions at any commit this increment.

**Explicitly not done yet this increment:** a true multi-hour soak
(bounded ~15-min-per-level run performed instead); periodic forced-
crash-during-soak testing (Section 12); dedicated pathological-WAL
recovery stress beyond Phase 0/1's existing coverage (Section 13); a
full production metrics layer (most of Section 15's counter list) and
its on/off performance comparison (Section 16); `cargo-audit`/
`cargo-deny` (neither installed — manual `Cargo.lock` review performed
instead); the 1,000-writer soak result and final post-hardening
benchmark comparison (in progress as of this entry). Full itemized
list, kept current: `PHASE3B_TEST_RESULTS.md`.

**Open Tier 3 question currently blocking further work:** none.

---

## 2026-09-16 (Phase 3, Increment 3B: 1,000-writer soak result + a genuine finding + final verdict)

**Implemented:** completes the prior entry. The 1,000-writer, 900-second
soak's write path finished cleanly — 84,877,639 ops, zero errors, zero
timeouts, flat RSS, no throughput-degradation trend, `pool.shutdown()`
returned `Stopped`/`fully_drained=true`. Immediately afterward, this
session's background task was **killed by the OS ("system is running
low on memory")** — not during the write path, but during the harness's
own post-run recovery-verification call
(`FileWal::open_for_recovery`), which was in the middle of
materializing all ~85M recovered records into one `Vec<(u64,
WalOpOwned)>` on a 16 GiB host.

**Investigated, not dismissed** (this project's own "do not dismiss
slow leaks/failures without investigation" standard): confirmed the
write path was never implicated (RSS was flat and stable for the
entire preceding 900s) via a supplementary shorter run (90s, ~8.5M
records), which completed its *entire* write-and-recovery cycle
cleanly — exact record count, zero corruption, gap-free sequences. The
100-writer run's own earlier 15.5M-record recovery had also already
succeeded (prior entry). **Root cause**: `FileWal::open_for_recovery`'s
existing (Phase 0) API materializes every record in memory at once —
there is no streaming/iterator recovery API in this crate — and the
memory this requires scales with WAL size; somewhere between 15.5M and
85M records exceeded what this specific host could hold for that one
call. This is a real, genuine finding about the existing recovery API's
scalability, **not a defect Phase 3B introduced and not a write-path or
coordinator correctness bug** — recorded precisely, not papered over
(`PHASE3B_TEST_RESULTS.md` §8, `PHASE3B_ADR.md` ADR-P3B-5).
`examples/soak_test.rs` now warns loudly before attempting this step on
a large run and documents the finding in its own module doc comment, so
a future session recognizes it immediately. Fixing the underlying
recovery API (a streaming/iterator redesign) is explicitly out of
Phase 3B's scope — deferred, named as relevant to whichever future
phase next touches WAL recovery internals (plausibly Stage B/MemTable's
own recovery reconstruction work).

Final post-hardening performance re-verification (100w/1000w, 3
repetitions each, same machine/binary/methodology as every prior
phase): 100 writers median 16,133 ops/sec (target ≥15,000, +7.6%
margin); 1,000 writers median 91,208 ops/sec (target ≥80,000, +14.0%
margin). Both fall inside the historical noise band established
*before* this run (`PHASE3B_TEST_PLAN.md` §1) and are not reproducibly
low across repetitions → **no regression from Phase 3B's hardening
work**, per the pre-established rule, not a post-hoc rationalization.

**Tests passing: VERIFIED**, unchanged from the prior entry — 128/128
across debug/release/test-util, clippy and fmt clean, crash-consistency
suite green.

**Final Phase 3B decision** (operating brief §29, full reasoning in
`PHASE3B_TEST_RESULTS.md` §11): **PHASE 3B INCOMPLETE — BLOCKERS
REMAIN.** Six explicit, named blockers, none of them an unrelated
future feature: no true multi-hour soak (a bounded ~15-min-per-level
run substituted); no periodic forced-crash-during-soak testing; no
dedicated pathological-WAL recovery stress beyond Phase 0/1's existing
coverage; no full production metrics layer (only a targeted audit plus
4 safe additions); `cargo-audit`/`cargo-deny` not run (neither
installed; manual review substituted); the recovery-API memory-scaling
finding above is documented but not fixed. Everything that *was*
attempted passed, with zero fabricated results and zero regressions.
Recommendation: the write-path/coordinator hardening delivered this
increment is safe to build on; Stage B (MemTable) should not begin
until a follow-up increment closes at minimum the soak-duration and
periodic-crash-testing blockers, since those are the evidence Stage
B's own correctness will need to be judged against.

**Open Tier 3 question currently blocking further work:** none — the
next increment (closing Phase 3B's remaining blockers, or beginning
Stage B against the user's own risk tolerance for the open items) has
no unresolved Tier 3 question yet identified.

---

## 2026-09-16 (Phase 3C: final WAL/coordinator release certification — in progress)

**Implemented:** targets exactly Phase 3B's own six named blockers
(`PHASE3B_TEST_RESULTS.md` §11). Froze the baseline at commit `4221e2f`
(clean tree): 100w median 17,872 ops/sec, 1000w median 91,517 ops/sec.

Added `GroupCommitter`/`BatchCoordinatorPool::purge_before` (mirrors
the existing `rotate()` wrapper exactly), enabling realistic bounded-
WAL checkpointing during a genuinely long soak — without it, a true
multi-hour run at full throughput would generate far more records than
the recovery-memory limitation (`PHASE3B_ADR.md` ADR-P3B-5) can safely
recover at the end. Launched a true 4-hour-per-writer-level soak
(`examples/long_soak_test.rs`, 100 writers then 1,000 writers) in the
background with periodic checkpointing — running as of this entry; see
`PHASE3C_TEST_RESULTS.md` §3 for its current, live status.

Closed, with real evidence, while the soak ran: periodic forced-crash-
during-soak testing (`examples/crash_cycle_test.rs` — spawns a real
child process, kills it externally at a randomized, seeded, reproducible
delay via `Child::kill()`, a genuinely new fault-injection class
distinct from every prior in-process mechanism since it is truly
asynchronous and uncooperative; 40/40 cycles recovered cleanly, zero
corruption, monotonic gap-free sequences — also the first real test,
under actual external-kill conditions, of this project's cross-process
file-lock release-on-death guarantee); pathological recovery stress
(`tests/pathological_recovery_matrix.rs`, 9 fixtures, 9/9 pass, against
the existing unmodified recovery contract — two fixtures are genuinely
new coverage beyond Phase 0/1's own extensive corruption testing: an
out-of-range length field at the recovery boundary specifically, and an
unrecognized op-tag byte with a recomputed valid CRC, isolating the
op-decode failure path from the CRC-mismatch path); the recovery-memory
finding quantified with real swept data (`examples/recovery_memory_
scaling.rs`, 1M/5M/10M/15M records: RSS scales linearly at ~134 bytes/
record, recovery throughput stays flat at ~184-186K records/sec
regardless of scale — confirms and precisely characterizes what was
previously a single anecdotal data point) and formally analyzed for a
future redesign (streaming iterator / callback-based replay / bounded
replay batches — `PHASE3C_ADR.md` ADR-P3C-1, analysis only, deliberately
not implemented this phase, per the operating brief's own explicit
instruction not to quietly change recovery semantics); two further
genuine, low-contention observability fields (`BatchCoordinatorStats::
bytes_total`/`avg_bytes_per_batch`, `writes_timed_out` — a real
sub-classification of `completed_err`, verified distinct from an fsync
failure by a dedicated test); a completed security/dependency review
(zero `unsafe` code and zero payload logging across every Phase 3C
addition; `Cargo.lock` fully reviewed, no new production dependency
this phase; `cargo-audit`/`cargo-deny` not installed — network access
to crates.io returned HTTP 403 in this environment, judged unreliable
to depend on, decision documented rather than silently skipped).

**A real methodological mistake made and corrected mid-session,
recorded honestly rather than hidden:** partway through this phase, a
benchmark run was attempted while the background long soak was still
actively running its own 100 concurrent writer threads — the resulting
numbers (~9,000-10,000 ops/sec, well below every historical baseline)
were an artifact of CPU contention on this machine's 4-physical/8-
logical-core hardware, not a code regression. Recognized before being
reported as evidence, discarded, and a new rule added to `PHASE3C_TEST_
PLAN.md` §1 (never benchmark concurrently with an active background
soak) before any further measurement was taken. The same contention
also produced one transient dip in the soak's own throughput/latency
data around t≈601-742s of the 100-writer run (ops/sec briefly down to
1,842, p99 up to ~795ms) — investigated, not dismissed: zero requests
were lost (`completed_err=0` throughout), and throughput/latency
recovered fully and immediately once the concurrent load ended,
recorded in `PHASE3C_TEST_RESULTS.md` §3 as evidence the system
degrades proportionally and recovers promptly under real external
contention, not as a defect.

**Tests passing: VERIFIED** (debug profile; release-profile and
`tests/group_commit`/`tests/crash_consistency` re-runs deferred until
the background soak's binaries are no longer running, to avoid both a
file-lock conflict and contaminating either measurement). `cargo test
--lib`: 130/130 (129 pre-existing + 1 new). `cargo test --lib
--features test-util`: 130/130. `cargo clippy --all-targets
--all-features -- -D warnings`: clean. `cargo fmt --check`: clean.
`cargo test --test pathological_recovery_matrix`: 9/9. Zero regressions
at any commit this increment.

**Explicitly not done yet this increment:** the long soak itself has
not yet completed (§3 of `PHASE3C_TEST_RESULTS.md` is a live, in-
progress section, updated in place as data arrives, not a placeholder);
the final post-hardening benchmark comparison and full release
regression gate are deferred until the soak finishes and the machine is
genuinely idle; the final certification decision (§26: **WAL FOUNDATION
CERTIFIED FOR LSM INTEGRATION** or **WAL FOUNDATION NOT YET CERTIFIED**)
is not yet recorded — `PHASE3C_TEST_RESULTS.md` explicitly defers it
rather than guessing ahead of the evidence. A production percentile
(`p50`/`p95`/`p99`) `commit_latency` metric remains unbuilt in the
library itself (a validated per-thread-local-slot design exists in the
soak harnesses, not yet wired in as a library feature) — `PHASE3C_
ADR.md` ADR-P3C-4.

**Open Tier 3 question currently blocking further work:** none.

---

## 2026-09-16 (Phase 4A: MemTable + RUBIC format foundation)

**Implemented:** began Phase 4A explicitly before Phase 3C's own WAL
certification had completed — its long soak was still running, launched
under commit `383cac7` — on the documented basis that Phase 4A touches
no WAL/`GroupCommitter`/`BatchCoordinatorPool` internals at all
(`PHASE4A_ARCHITECTURE.md` §0). The soak continued running, healthy,
throughout all of this phase's own work (last checked at t≈4,111s of
its 14,400s first phase: 17,067 ops/sec, RSS flat ~8.7 MB, zero errors).

Wrote `RUBIC_FORMAT_SPECIFICATION.md` first, per the operating brief's
own "define before implementing persistent storage" instruction — a
family-policy/governance document, not a reinvention of the already-
specified RUBIC SSTable byte layout (`RubixDB-LSM-Engine-Specification-
v1.0.md` §2, "Status: Final," referenced not duplicated) and explicitly
not a renaming of the existing WAL format (an already-established
compatibility boundary — any future formal absorption into the RUBIC
family would need its own separate, versioned decision).

Implemented `src/memtable/mod.rs` exactly per the existing, final LSM
Engine Spec §1 — `BTreeMap<(Vec<u8>, u64), MemtableValue>`, `get_as_of`
via `range(...).next_back()`, documented size accounting
(`ENTRY_OVERHEAD = 32`), and the compile-time-enforced `freeze() ->
Arc<MemTable>` pattern (no `&mut` method reachable on the frozen
handle — Rust's own ownership rules, not a runtime flag). No `SkipList`
evaluation performed: the spec is "Final," prescribes `BTreeMap` by
exact type, and leaves no degree of freedom to evaluate against it. 13
unit tests (every item in the spec's own §1.6 checklist) plus 2
property tests (1,000 cases each) comparing against a deliberately
naive reference model.

Added `wal::replay_streaming` (`src/wal/mod.rs`) — a new, additive,
bounded-memory WAL replay API implementing the callback-replay
direction `PHASE3C_ADR.md` ADR-P3C-1 already analyzed but did not build.
`open_for_recovery`/`WalReplayResult`/`walk_segment`/`scan_directory`
are byte-for-byte unchanged (verified: the full pre-existing 90-test WAL
suite passes unmodified). **A real bug found and fixed while wiring
this up**: the first version failed on a brand-new engine's not-yet-
created WAL directory, and calling `open_for_recovery` first to fix
that was not viable either — its exclusive lock blocks `replay_
streaming`'s own shared-lock attempt, even from the same process (this
project's own cross-process-locking design, verified when that
mechanism was first built in an earlier phase). Fixed by treating a
not-yet-existing directory as "nothing to replay," matching `scan_
directory`'s own existing-empty-directory behavior.

Implemented `src/lsm/mod.rs` (`LsmEngine`) — the Phase-4A-scoped write-
path facade (no `sstables`/`manifest`/compaction, extended in Phase
4B): `put`/`delete`/`get`/`get_as_of`, wired to the unmodified
`BatchCoordinatorPool` for durability and `MemTable` for state,
enforcing the exact WAL-durability-before-MemTable-apply ordering.
**Corrected a design claim mid-implementation, before it became load-
bearing**: an earlier architecture-doc draft assumed only the
coordinator thread would ever call `MemTable::insert`; the actual
design applies each entry on whichever caller thread's own `Completion::
wait()` returns, synchronized by `RwLock<MemTable>` rather than thread
affinity — still correct, because `(user_key, seq)` keys are globally
unique, so concurrent inserts of different keys commute regardless of
application order. Verified by concurrency tests at 1/10/100/1,000
logical writers through the real, unmodified `BatchCoordinatorPool`
(operating brief §31's own explicit requirement), all passing with zero
lost/duplicate records.

Built real crash tests at the WAL/MemTable boundary
(`examples/lsm_crash_cycle_child.rs`/`lsm_crash_cycle_test.rs`),
mirroring Phase 3C's own proven external-process-kill design
(`PHASE3C_ADR.md` ADR-P3C-3) but pointed at `LsmEngine`: 25/25 cycles
recovered cleanly, and at **every single cycle**,
`active_entries == highest_sequence == durable_through` exactly —
the strongest evidence this phase has that the recovered MemTable
always contains precisely the WAL's own durable record count, under
real abrupt kills, not just a unit-test-level fsync-failure injection.

Measured MemTable-only performance cleanly (`examples/memtable_bench.rs`,
single-threaded, not meaningfully affected by the concurrent background
soak): 1.45M puts/sec, get p50=400ns/p99=1,200ns, ~74M entries/sec
ordered iteration. **Did not** measure the full multi-threaded WAL-vs-
WAL+MemTable comparison cleanly — a 20-writer smoke attempt
(`examples/lsm_load_test.rs`) was visibly contaminated by the
concurrently-running background soak (1,479 ops/sec, an order of
magnitude below expectation) and was explicitly discarded as evidence,
not reported as a real number (`PHASE4A_ADR.md` ADR-P4A-6) — the same
contamination-avoidance discipline Phase 3C's own `PHASE3C_TEST_PLAN.md`
§1 rule 5 already established.

**Tests passing: VERIFIED.** `cargo test --lib`: 170/170 (130
pre-existing + 40 new across MemTable/replay_streaming/LsmEngine).
`cargo test --lib --features test-util`: 170/170. `cargo clippy
--all-targets --all-features -- -D warnings`: clean. `cargo fmt
--check`: clean. Zero regressions at any of the 11 commits this phase.
`cargo test --release --lib` was not re-run in isolation at the end of
this phase (deferred alongside the performance comparison to avoid the
background soak's own release-binary conflict) — recorded as an open
item, not silently skipped.

**Final decision** (`PHASE4A_TEST_RESULTS.md` §12, stated per operating
brief §42's own "do not certify based only on unit tests" instruction):
**MEMTABLE NOT YET READY FOR RUBIC SSTABLE IMPLEMENTATION — BLOCKERS
REMAIN.** Two explicit blockers, neither a correctness defect: the full
WAL-vs-WAL+MemTable performance comparison is not run; Phase 3C's own
WAL certification had not completed when this phase's work concluded.
Every correctness/durability/crash-recovery/concurrency property
actually tested this phase passed cleanly and does not need to be
redone once those two items close — the recommended next step is
closing them (let the Phase 3C soak finish, then re-run the deferred
comparison on the resulting idle machine), not redoing correctness work.

**Open Tier 3 question currently blocking further work:** none.

---

## 2026-09-17

**Implemented:** RUBIC SSTable (Phase 4B) — the immutable, persistent,
sorted on-disk table format, its writer/reader, and the flush
integration into `LsmEngine`, per `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`
and `PHASE4B_ARCHITECTURE.md`.

Before any code: inspected `PHASE3C_TEST_RESULTS.md` and
`PHASE4A_TEST_RESULTS.md` directly rather than assuming their status —
found Phase 3C still explicitly "Deferred" and Phase 4A explicitly
"NOT YET READY — BLOCKERS REMAIN," and proceeded on that documented,
provisional basis (`PHASE4B_ADR.md` ADR-P4B-0), the same posture Phase
4A itself used against a then-incomplete Phase 3C.

**The one genuine stop-and-ask decision this phase**: the already-final
`RubixDB-LSM-Engine-Specification-v1.0.md` ties safe WAL-purge/replay-
boundary behavior to the Manifest's `SET_CHECKPOINT` edit, but Manifest
is explicitly out of scope this phase. Asked the user directly rather
than silently choosing; the answer selected was: SSTable is a purely
additional, purely derived read-path source this phase — the WAL is
never purged or truncated by a flush, and recovery keeps doing the
exact full `wal::replay_streaming` reconstruction Phase 4A already did,
unchanged (`PHASE4B_ADR.md` ADR-P4B-1). This has a valuable, direct
safety corollary: a corrupt or missing SSTable can never cause data
loss this phase, because the WAL, untouched, still holds everything —
only that one file's read-path *availability* is at risk, so
`LsmEngine::open` fails closed on a corrupt discovered SSTable
(ADR-P4B-2) rather than silently degrading.

Wrote `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` consolidating the
already-final byte layout (magic `"RBXSST01"`, CRC32C, 4096-byte target
blocks, 10-bits/key bloom filter, 72-byte footer — all already decided
by the LSM spec, not re-derived) plus the Manifest-free resolutions this
phase actually needed: directory-scan-based SSTable id recovery,
"exists and validates" as the liveness rule, and reuse of the WAL's own
already-tested `fsync_dir` platform primitive (one-line `pub(crate)`
visibility export, `PHASE4B_ADR.md` ADR-P4B-4) rather than a second,
divergent Windows/Unix implementation.

Implemented `src/sstable/` (`format.rs`, `bloom.rs`, `writer.rs`,
`reader.rs`): byte-exact encode/decode for every structure, a bloom
filter using the spec-mandated XXH64 double-hashing scheme (new
dependency `xxhash-rust`, pure Rust, zero transitive deps — the only
new dependency this phase, `PHASE4B_ADR.md` ADR-P4B-3), an atomic
tmp-file-then-rename writer, and a bounded-memory reader (footer/index/
bloom eager, data blocks lazy, positional reads so concurrent readers
never need a lock on the shared file handle). Wired into `LsmEngine`
(`src/lsm/mod.rs`): a new background flush thread drains `immutables`,
publishes SSTables, and extends the read path (`get_as_of`, now
fallible — a real, necessary API change once SSTable reads can fail
closed on corruption) to check `active` -> `immutables` -> `sstables` in
strict recency order.

**A real correctness bug found and fixed by this phase's own property
test**: `SsTable::get_versioned`'s original `Vec::binary_search_by`
does not guarantee finding the *leftmost* match when consecutive blocks
share an identical `last_key` (a key's version run spanning a block
boundary) — it silently skipped earlier blocks holding older versions
of the query key. `sstable::tests::property::
sstable_matches_memtable_reference` (a 64-case proptest against a
`MemTable` reference) caught this directly; fixed by switching to
`Vec::partition_point`, and a matching over-strict "last_key must be
strictly ascending" validation bug in `format::decode_index_block` was
found and relaxed to "non-decreasing" at the same time. Both are
recorded in detail in `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.6 and
`PHASE4B_TEST_RESULTS.md` §7 so neither is ever silently reintroduced.

Built real crash tests for the flush pipeline
(`examples/sstable_flush_crash_child.rs`/`sstable_flush_crash_test.rs`),
the same external-process-`Child::kill()` methodology every prior
phase's own crash-cycle harness uses, but with a deliberately tiny
memtable/block configuration so kills land throughout the flush state
machine across many cycles: **140/140 cycles across two seeds, zero
failures** — `open()` never errored, no orphaned `.sst.tmp` ever
survived the discovery sweep, and the durability watermark never
regressed.

Measured performance cleanly (machine verified idle first): SSTable
write 102.40 MB/sec / 1.38M records/sec, point lookup p50=12µs/p99=34µs,
ordered iteration ~5M records/sec. Closed the exact gap Phase 4A's own
`ADR-P4A-6` left open — the integrated WAL-only vs. WAL+MemTable vs.
WAL+MemTable+SSTable-flush comparison, at 100 and 1,000 writers: under
the LSM spec's own realistic 4 MiB default memtable, flush overhead at
1,000 writers is within noise of the WAL-only baseline (98,666 vs.
97,564 ops/sec); under a deliberately tiny 65,536-byte memtable (a
stress configuration producing 622 SSTables from the same 1,000,000
records), flush's real disk-I/O contention cost becomes clearly
measurable (56,609 ops/sec) — reported as a configuration guideline,
not a defect. One performance oddity (100-writer, small-memtable
"flush faster than no-flush") is recorded as an open, not-fully-
explained item rather than picked apart with a story that isn't fully
supported by evidence (`PHASE4B_PERFORMANCE.md` §3.3/§5).

**Tests passing: VERIFIED.** `cargo test --lib`: 216/216 (170
pre-existing + 46 new: 43 in `src/sstable/`, 3 new `LsmEngine` flush-
integration tests, plus 2 pre-existing tests updated to use a new
test-only flush-delay hook rather than left racy against the new
background flush thread). `cargo test --lib --features test-util`:
216/216. `cargo test --release --lib`: 216/216. `cargo clippy
--all-targets --all-features -- -D warnings`: clean. `cargo fmt
--check`: clean.

**Final decision** (`PHASE4B_TEST_RESULTS.md` §10): **RUBIC SSTABLE
READY FOR MANIFEST**, conditioned — exactly as Phase 4A's own
certification was — on Phase 3C's long-soak certification eventually
landing clean; that item remains open and independent of this phase's
own work.

**Open Tier 3 question currently blocking further work:** none.

---

## 2026-09-17 (continued)

**Implemented:** RUBIC Manifest (Phase 5) — the crash-safe record of
the live SSTable set and the durable checkpoint boundary, and the safe
WAL retention/purge mechanism it authorizes, per `RUBIC_MANIFEST_
FORMAT_SPECIFICATION.md` and `PHASE5_MANIFEST_ARCHITECTURE.md`.

Ran the release-gate audit first, as required: launched Phase 3C's
still-outstanding long soak, then caught and corrected a real
sequencing mistake within the same session (killing leg 1 early caused
the wrapper script to advance straight into leg 2, which would have
contaminated every one of this phase's own required clean benchmarks —
`PHASE5_ADR.md` ADR-P5-0) — stopped it, ran every clean measurement
this phase needed, and relaunched it uninterrupted as the very last
action. Built a dedicated ablation (`examples/freeze_ablation_test.rs`)
for the still-unexplained Phase 4B 100-writer anomaly: conclusively
ruled out the bounded-`BTreeMap`-depth hypothesis (a freeze-and-
discard-only variant never measured faster than the no-freeze
baseline), left the real mechanism honestly unexplained rather than
guessed at.

The Manifest format was already fully specified (LSM Engine Spec §6.1
— three edit types, WAL-frame-format reuse) — no format ambiguity to
resolve. The genuinely hard design work was the *integration*:
reconciled an apparent two-level description gap between the spec's
§3.2 (atomic SSTable construction, including its own `ADD_SSTABLE`
Manifest step) and §4.4 (the higher-level flush pseudocode, which
doesn't re-show that step) as one indivisible unit, not a
contradiction; derived a two-phase Manifest-recovery startup sequence
(`manifest::replay_readonly` under a shared lock, before the exclusive
lock; `Manifest::open_after_exclusive_lock` after it) that preserves
Phase 4A's existing `replay_streaming`-before-`open_for_recovery`
lock-ordering constraint without changing that WAL function's own
signature at all.

Implemented `src/manifest/` (`format.rs`, `state.rs`, `recovery.rs`,
`mod.rs`): an independent (not shared-code, but byte-compatible) frame
implementation reusing the WAL's own frame-header shape, sequential
bounded-memory replay with the identical torn-vs-corrupt classification
the WAL uses for its own segments, and idempotent recovery semantics
exactly matching the spec's own tolerance rules. Wired the WAL's
already-existing but previously-inert `CHECKPOINT_MARKER` op into real
use for the first time since Phase 4A defined it. Extended `LsmEngine`'s
flush pipeline to the full ten-step publish -> checkpoint -> purge
sequence the WAL and LSM specs jointly require, reusing existing,
already-tested primitives throughout (`pool.rotate()`, `pool.submit
(CheckpointMarker)`, the existing `purge_before`) rather than inventing
anything new.

**A real idempotent-retry bug found by this phase's own crash-cycle
testing, not by inspection**: the first working retry design tracked
only whether the SSTable itself had been built, so a failure *after*
the `CHECKPOINT_MARKER` had already been durably written (but before
`SET_CHECKPOINT` landed) caused a retry to durably resubmit a *second*
marker for the same logical flush. Found because the crash test was
extended to assert an *exact* accounting invariant
(`active_entry_count() + recovery_stats().checkpoint_markers_replayed
== highest_seq - checkpoint_seq`) rather than a loose bound — that
invariant failed on 94 of the first 100 real crash cycles against the
initial design. Fixed by tracking each step's own durable-success state
independently; re-verified clean, 180/180 cycles across two seeds,
after the fix (`PHASE5_ADR.md` ADR-P5-4).

Audited the flush-thread-panic gap `PHASE4B_FAILURE_MODEL.md` had
already named as newly more consequential once WAL retention depends on
flushing continuing. Decided against a supervised-restart thread design
(the operating brief's own named risk: could duplicate an SSTable or
replay an unsafe checkpoint transition) in favor of `catch_unwind`
around each flush attempt, treating a caught panic identically to an
I/O failure through the same already-proven idempotent-retry machinery
— the thread itself never dies, so there is no restart state to
reconcile (`PHASE5_ADR.md` ADR-P5-5).

Added `LsmEngine::recovery_stats()`/`checkpoint_seq()`/
`manifest_record_count()`/`manifest_size_bytes()`/`manifest_last_edit()`/
`live_sstable_ids()` — real observability, not a checklist exercise
(`recovery_stats()` specifically exists because the crash test's own
exact-accounting invariant needed it to find the bug above).

Ran a bounded (~3 minute, not multi-hour) soak with periodic real
process kills interleaved (`examples/manifest_soak_test.rs`): 8/8
cycles clean, and — the property this soak specifically exists to
demonstrate — WAL byte count stayed at exactly `0` across every
measurement (this tiny-memtable stress workload's checkpoint tracks
within ~1% of `highest_seq` at all times, so `purge_before` reclaims
essentially the whole WAL every cycle), while Manifest/SSTable-
directory size grew as expected in the explicit absence of Compaction.

Measured performance cleanly: at 1,000 writers under a realistic 4 MiB
memtable, the full Manifest-integrated pipeline (96,368 ops/sec)
measured within ~2% of a clean WAL-only baseline (98,225 ops/sec) —
checkpoint/purge overhead is negligible at realistic configuration.

**Tests passing: VERIFIED.** `cargo test --lib`: 253/253 (216
pre-existing + 37 new: 32 in `src/manifest/`, 5 new `LsmEngine`
Manifest-integration tests). `cargo test --lib --features test-util`:
253/253. `cargo test --release --lib`: 253/253. `cargo clippy
--all-targets --all-features -- -D warnings`: clean. `cargo fmt
--check`: clean.

**Final decision** (`PHASE5_TEST_RESULTS.md` §10): **MANIFEST NOT
READY FOR COMPACTION — BLOCKERS REMAIN.** Two explicit blockers,
neither a correctness defect: Phase 3C's own WAL certification has
still never completed (carried forward, unresolved, across four
phases now); the true multi-hour, realistic-configuration Phase 5 soak
is not complete (only a bounded stress-configuration soak and the
relaunched-but-still-running Phase 3C soak exist as evidence). Every
correctness property actually tested this phase passed cleanly and
does not need to be redone once those two items close.

**Open Tier 3 question currently blocking further work:** none.

## 2026-09-19 (Write-Engine Certification: realistic full-pipeline soak run — new blocking finding)

Phase 3C's own WAL long-soak certification (the blocker carried since
Phase 3C→4A→4B→5, see `PHASE_WRITE_ENGINE_TEST_RESULTS.md` §7) and the
short build/test/clippy/security/property-test gates all completed
cleanly earlier in this certification effort. The one remaining gap —
"the true multi-hour, realistic-configuration full-pipeline soak" named
above as still outstanding — was run for the first time: `examples/
realistic_full_pipeline_soak.rs`, 200 writers, `LsmConfig::default()`,
target 14,400s (`temp/long_soak_logs/realistic_soak_200w_20260919_
085109.*`).

**Explicitly flagged, not silently resolved:** the harness that ran it
reported `REALISTIC FULL-PIPELINE SOAK RESULT: PASS` on `exit_code==0`
+ clean process tree alone. That is not a valid soak pass. The run's
own data shows the target volume (this machine's chronically
~97%-full `C:` `%TEMP%`, already documented in `FINAL_WAL_ANALYSIS.md`
§5/`PHASE1_TEST_RESULTS.md` §9C.4) filled at t≈5,100s, after which
`completed_err` climbed to 580,190,298, throughput collapsed 97.9%,
and stderr logged 4,407 `os error 112` (ENOSPC) flush failures — the
background flush thread's retry loop (`src/lsm/mod.rs:913-1037`) is
unbounded past `max_flush_retries` (that config value only picks a
backoff duration, never a stop condition), so the engine spent the
remaining ~9,200s of the 4-hour run retrying a doomed flush every 2s
instead of failing safe or signaling backpressure. Durability and
crash-recovery correctness were unaffected — post-shutdown reopen
recovered cleanly, 0 corruption, all 1,401 live SSTables reconciled
against the Manifest. Full timeline, root-cause citation, and a
from-source proof that the `completed_err` figure is not an accounting
bug (`submitted - completed_ok - completed_err == writer_count`,
verified exactly): `PHASE5_ENOSPC_FAILURE_ANALYSIS.md`.

**Fixed as part of this same finding:** the soak harness itself
(`temp/realistic_soak_harness.ps1`) now requires `completed_err == 0`,
bounds on consecutive-collapsed-throughput samples, zero ENOSPC/retry-
storm lines in stderr, a `recovery OK` line, and a fully-drained clean
shutdown before reporting PASS — replaying the corrected logic against
this same run's evidence now correctly yields FAIL. `PHASE_WRITE_
ENGINE_TEST_RESULTS.md` §7a/§12/Summary amended to record the new
verdict rather than "NOT RUN."

**Not yet done, deliberately:** the retry-policy fix and the
`HEALTHY → STORAGE_PRESSURE → STORAGE_FULL` state model this finding
calls for are a genuine design decision on the frozen write path, not
a mechanical patch — consistent with this project's own convention of
an ADR before a write-path behavior change (`PHASE4A_ADR.md`,
`PHASE5_ADR.md`'s idempotent-retry fix), that design has intentionally
not been implemented inline inside the failure analysis. The realistic
full-pipeline soak must not be re-run until that design lands, is
implemented, and has its own capacity-exhaustion + crash-under-ENOSPC
tests passing (`PHASE5_ENOSPC_FAILURE_ANALYSIS.md` §6, §9's referenced
brief).

**Final decision:** **WRITE ENGINE NOT READY — BLOCKERS REMAIN.** The
blocker is no longer "full-pipeline soak not run" (it has been); it is
now "full-pipeline soak run, found unbounded ENOSPC retry with no
backpressure or storage-pressure signal, not yet fixed or re-verified."

**Open Tier 3 question currently blocking further work:** the storage-
pressure/retry-policy ADR named above — ready to design, not yet
started.

## 2026-09-19 (continued: ADR-WE-SP-001 designed and implemented)

The storage-pressure/retry-policy ADR named above (`PHASE_WRITE_ENGINE_
STORAGE_PRESSURE_ADR.md`, ADR-WE-SP-001) was written, approved, and
implemented the same day. Full detail (exactly what landed vs. what the
ADR describes, including two deliberate scoping decisions — non-ENOSPC
I/O errors keep the old flat-2s-forever cadence since this ADR targets
storage-capacity exhaustion specifically, and the platform free-space
pre-check was not implemented since it would require a new dependency)
is in that document's own "Implementation Notes" section, not
duplicated here per this project's own convention (`PHASE_TEST_RESULTS.md`
is the source of truth for pass/fail data; ADR documents are the source
of truth for their own implementation notes).

Summary: `EngineError::StorageExhausted` (new, additive error variant),
`lsm::StorageState` (`Healthy`/`StoragePressure`/`StorageFull`, `AtomicU8`-
backed on `LsmEngine`), and a corrected flush-thread retry loop
(`src/lsm/mod.rs`) that now genuinely bounds the fast-retry phase by
`max_flush_retries` and, on a confirmed ENOSPC-classified failure past
that budget, backs off at the new, slower `storage_pressure_retry_
interval` instead of the old unconditional flat-2s-forever cadence that
caused the 2026-09-19 realistic soak's 9,200-second retry storm
(`PHASE5_ENOSPC_FAILURE_ANALYSIS.md`). `LsmEngine::put`/`delete` now
reject fast with `StorageExhausted`, before any WAL append, once
`StorageFull` is confirmed (the immutable backlog reaching its existing
`max_immutable_memtables` bound while already stuck in
`StoragePressure`) — the pre-existing `CapacityExceeded` contract
(`[[project_rubixdb_capacity_contract]]`) is completely unchanged.

Two new tests, both actually run and verified (not just written):
`lsm::tests::storage_pressure_state_machine_recovers_after_injected_
enospc` (in-process, a new `install_flush_io_fault_hook` fault-
injection point extends the existing `FlushFaultPoint` pattern to
substitute a real ENOSPC-shaped `io::Error`, never touching real disk
capacity — stable across 5 consecutive runs) and `examples/
storage_pressure_crash_{child,test}.rs` (external-process, `Child::
kill()` synchronized to a marker line the child prints on reaching
`StorageFull`, not a blind wall-clock delay — stable across 10
consecutive cycles after fixing two real bugs *in the test itself*
caught by actually running it before trusting it: a shared-directory-
across-cycles bug that let a later cycle inherit an earlier cycle's own
legitimate progress, and an over-strict treatment of the pre-existing
`CapacityExceeded` contract as a test failure).

**Full regression suite, run and verified:** `cargo test --lib`: 255/255
(254 pre-existing + 1 new). `cargo test --release --lib`: 255/255.
`cargo test --lib --features test-util`: 255/255. `cargo clippy
--all-targets --all-features -- -D warnings`: clean. `cargo fmt --check`:
clean. **Flagged, not silently ignored:** one pre-existing test,
`execution::batch_coordinator::tests::coordinator_panic_before_batch_
formation_fails_safely` (a file this work never touched), failed
intermittently under full-suite parallel load both before and after
this change and passed reliably alone or on a clean rerun — a
pre-existing flake, not attributed to this work, not fixed here
(out of scope).

**Not done, deliberately, per the ADR's own §19 instruction:** the
realistic full-pipeline endurance soak has not been rerun yet. Next
steps are the storage-budget calculation and provisioning a dedicated,
sufficiently large test volume (not the chronically near-full `C:`
`%TEMP%` the original failing run used), then the re-run, then the
final 100w/1000w performance acceptance comparison.

**Final decision:** still **WRITE ENGINE NOT READY — BLOCKERS REMAIN**,
but the blocker has narrowed from "ENOSPC causes an unbounded retry
storm with no backpressure" (fixed, tested, verified above) to "the
corrected engine has not yet been re-verified under the original
realistic full-pipeline soak workload on a properly provisioned
volume."

**Open Tier 3 question currently blocking further work:** none — next
step (storage budget + dedicated volume + re-soak) is mechanical, not a
design decision.

## 2026-09-20 (Write-Engine Certification: storage budget, E: re-soak, final decision — WRITE ENGINE PRODUCTION READY)

Executed the mechanical next step named above, in full, before launching
anything: measured `E:` (94.66 GB free, healthy NTFS — `Get-Volume`/
`Get-CimInstance Win32_LogicalDisk`), computed a conservative storage
budget from the original failed run's healthy-period SSTable growth
rate (~35.78 GB required with a 2x safety margin, 164.6% headroom —
`PHASE_WRITE_ENGINE_STORAGE_BUDGET.md`), made the smallest safe harness-
only fix to force the soak's database directory onto `E:`
(`RUBIXDB_SOAK_BASE_DIR`, honored by a new `soak_base_dir()` helper in
`examples/realistic_full_pipeline_soak.rs`, falling back to the old
default when unset), and verified that fix end-to-end through the exact
`Start-Process` invocation pattern the harness itself uses before
trusting it. Ran two smoke-test soaks (20 writers; extended from the
requested 30s to 60s since 30s didn't reach a freeze at the production
4 MiB memtable -- flagged, not silently kept as a smoke test that
wouldn't actually exercise freeze/flush/checkpoint), with live mid-run
filesystem inspection directly confirming WAL/MANIFEST/SSTables
physically on `E:`. Re-verified the ADR-WE-SP-001 storage-pressure fix
fresh (5/5 in-process, 10/10 external crash cycles) and the full
regression gate (fmt/clippy/`cargo test --lib`×2/`--features
test-util`×2, 255/255 each) immediately before launch.

Launched the real 200-writer, 14,400s soak on `E:` in the background.
It ran the full duration (23:47 → 03:47) and **genuinely passed**:
`completed_err=0` the entire run, throughput sustained 19,332-26,116
ops/sec (mean 21,994) with no collapse, 3,294 SSTables published,
checkpoint advancing continuously, WAL bounded (3.59-6.94 MB), 0 ENOSPC
events (peak usage ≈8.86 GB against 94.66 GB available -- the storage-
pressure state machine was never even triggered), clean final recovery
(3,294 live SSTables reconciled, 6,588 Manifest records, 0 corruption).
`E:` free space was sampled every 60s throughout via a separate
monitoring job and never dropped below ≈85.8 GB.

**A second real bug was caught and fixed, this time in the harness
itself, by actually running it rather than trusting its first verdict:**
the harness's own PowerShell recovery/shutdown checks used
`$lines -notmatch "X"`, which does not mean "no line matches X" -- it
filters the log's ~135 lines down to the ones that *don't* match,
which is essentially always a large, truthy, non-empty array regardless
of whether the pattern was actually present. This produced a false FAIL
on the very first pass despite the underlying log genuinely containing
both `recovery OK` and `fully_drained=true`. Fixed
(`-not ($lines -match "X")`), and the fix itself was verified -- not
just asserted -- by replaying the corrected logic against *both* this
new passing run (correctly → PASS) and the original 2026-09-19 08:51
failed run (correctly still → FAIL, for the real ENOSPC reasons) before
trusting it. Original result-file evidence was preserved unmodified;
the correction is an appended entry (`temp/long_soak_logs/
realistic_soak_result_20260919_234720.txt`), not an overwrite.

Also ran fresh 100w/1000w full-pipeline acceptance benchmarks
immediately after the soak (`lsm_flush_load_test`, 3 reps each): 100w
median 16,601 ops/sec (target ≥15,000, PASS); 1000w every rep cleared
80,000 (84,295-92,001, median 86,758 -- PASS, unlike the original
post-8-hour-soak measurement that dipped to 66,535). This, combined
with the already-existing `PHASE3C_CLEAN_MACHINE_REMEASUREMENT.md`
clean-machine result, supersedes the earlier "1000-writer target NOT
MET" finding -- it was a post-soak machine-state artifact, not a code
regression.

Updated every doc named in this certification's own §15 requirement:
`PHASE5_TEST_RESULTS.md`/`PHASE5_PERFORMANCE.md` (pointer notes, not
rewritten history), `PHASE_WRITE_ENGINE_TEST_RESULTS.md` (§7a/§9/
Summary amended), `PHASE_WRITE_ENGINE_PERFORMANCE.md` (2026-09-20
update section), `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md`
(Implementation Notes closed out), and a new
`PHASE_WRITE_ENGINE_CERTIFICATION.md` -- the final certification
decision document, referenced by every other doc since 2026-09-18 but
never actually created until now.

**Final decision: WRITE ENGINE PRODUCTION READY.** Every mandatory
evidence category (correctness, durability, crash safety, recovery,
bounded resources, backpressure, storage-pressure handling,
concurrency, long-duration stability, performance, security,
observability, reproducible evidence) is independently evidenced, per
`PHASE_WRITE_ENGINE_CERTIFICATION.md`. Per this project's own stop
condition: do not proceed to the Read Engine, Compaction, Replication,
or multi-node work as part of this same task -- each starts as its own
separately-scoped phase.

**Open Tier 3 question currently blocking further work:** none. The
non-blocking gaps carried forward (RSS growth from pre-Compaction
SSTable-handle accumulation, unresolved performance-variance root
cause, one pre-existing unrelated flaky test) are documented in
`PHASE_WRITE_ENGINE_CERTIFICATION.md` §5, none rise to a blocker.

## 2026-09-20 (continued: the RSS growth flagged above was actually investigated before re-certifying)

The previous entry's certification was held back same-day: it had
certified **WRITE ENGINE PRODUCTION READY** on the strength of the
passing `E:` soak alone, while that same soak's own ~584 MB RSS growth
(+2,334.1% by start-vs-end) had only been asserted as "expected," not
investigated. Per this project's own rigor convention (never assert a
number without evidence), that was corrected before letting the
certification stand.

**Investigation performed** (`PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md`):
extracted the full 121-sample RSS series from the original soak (not
just start/end) — RSS is monotonically non-decreasing and tracks
`sstable_count` in exact lockstep (the final two samples, where
`sstable_count` stopped growing, show RSS also frozen at the identical
value). Ran a second, independent, bounded measurement (not another
4-hour soak): 1,000 writers, 3,000s, same production config, on `E:`,
with checkpoints recorded at 100/500/1,000/2,000+ SSTables. Both
datasets fit a near-perfect linear model against SSTable count
(`rss_kb = 43,870 + 177.56 × sstable_count`, **R²=0.9999**); extrapolating
this second run's fit to 3,294 SSTables predicts 628,745 KB against the
first run's actually-observed 608,824 KB — a 3.3% residual, i.e. fully
explained. **A real bug in the monitoring setup was caught before
trusting the scaling run's timing**: a bash-side `kill -0 <pid>` wait
returned a false "process exited" almost immediately, because Git
Bash's PID namespace does not reliably see native Windows processes —
confirmed still-running via `Get-Process` before treating the run as
complete, and every subsequent wait used PowerShell instead.

Traced ownership to source rather than speculating: `SsTable::open()`
(`src/sstable/reader.rs`) retains `bloom: BloomFilter` and
`index: Vec<IndexEntry>` in memory for every open table's entire
lifetime (data blocks are deliberately never loaded — bounded memory by
design), and nothing currently removes an SSTable from that set (no
Compaction yet). `ManifestState` (the potentially-large in-memory
replay structure) was confirmed, by grep, to be local to
`LsmEngine::open()` only — never stored on `LsmEngine`, ruled out as a
growth source. A real SSTable's on-disk footer was parsed directly
(not estimated): Bloom filter 125.41 KB (exactly matching
`bloom_bits_per_key=10` at that table's actual 102,721-record count),
index 15.13 KB — consistent with the measured ~176-178 KB/table slope
once ordinary Rust/Windows small-allocation overhead for the index's
per-entry owned key `Vec<u8>`s is accounted for. No other growing
structure was found anywhere in the write path (flush queue, batch
coordinator queue, WAL/GroupCommitter all confirmed bounded).

**`sync_failures=1` also traced to source**, not just noted:
`GroupCommitStats::sync_failures()` computes `sync_attempts -
sync_successes` from two independently-incremented atomics (attempt
counted before the fsync call, success counted only after) — a stats
snapshot taken while one fsync is mid-flight reads a phantom "failure"
that resolves itself. Confirmed against the data: the value only ever
took 0 or 1 across all 121 samples, oscillating, never accumulating;
`completed_err` stayed 0 and recovery found 0 corruption, both
inconsistent with a real unretried failure. No source change made (no
defect found).

**Decision: (A) EXPECTED BOUNDED METADATA GROWTH.** No code fix
required. The already-completed `E:` soak stands as valid endurance
evidence — no re-run needed, per this investigation's own instruction
that a re-run is only warranted if a production-code fix changed the
write path (it did not).

**Harness improved**: `min_rss_kb_observed`/`max_rss_kb_observed` added
to `examples/realistic_full_pipeline_soak.rs`'s summary output
(harness-only change, verified via a real smoke run). **Regression test
added**: `lsm::tests::sstable_count_and_immutable_memory_track_flushes_
exactly_no_extra_retention` locks in the two ownership invariants that
actually prevent a real leak (immutable bytes return to exactly 0 after
every flush; `sstable_count` grows by exactly 1 per successful flush,
never more or less) — stable across 5 runs.

**Performance re-examined honestly rather than trusted from one set**:
the original 3-rep 100w/1000w set (which happened to clear both
targets cleanly) was followed by two more 3-rep sets the same day, run
immediately after the memory-scaling test and its own heavy 1,000-
writer/3,000s load — i.e., explicitly not an idle machine.
**Combined 1,000w: 9 reps, range 64,518-96,950, median 84,295 (5/9 clear
the 80,000 target).** **Combined 100w: 6 reps, range 11,369-17,199,
median 16,841 (4/6 clear the 15,000 target).** Reported in full, not
filtered to the favorable set — this is the same already-documented,
pre-existing, non-blocking variance characteristic
`PHASE3C_CLEAN_MACHINE_REMEASUREMENT.md` first flagged before this
session began, reproduced again here, not newly discovered and not
attributed to a code regression. A genuinely idle machine was not
available in this automated session to resolve it further.

**Full regression gate re-run and verified clean**: `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test --lib`/`--release --lib`/`--features test-util`/`--release
--features test-util` (256/256 each), `wal_tests` (12/12),
`crash_consistency --features test-util` (2/2),
`pathological_recovery_matrix` debug+release (9/9 each), the storage-
pressure test (5/5, fresh) and crash-under-storage-pressure test (10/10,
fresh).

**Final decision, now backed by an actual investigation of every
flagged finding rather than an assumption: WRITE ENGINE PRODUCTION
READY.** Full gate-by-gate matrix (16 gates, all PASS, each with cited
evidence): `PHASE_WRITE_ENGINE_CERTIFICATION.md`. Per this project's own
stop condition: not proceeding to Read Engine, Compaction, Router,
Replication, or multi-node work — that starts as its own separately-
scoped phase, only when explicitly instructed.

**Open Tier 3 question currently blocking further work:** none.

## 2026-09-20 (Read Engine: architecture report, ADR-RE-001, Implementation Increment 1)

With the Write Engine certified, started the Read Engine as its own
separately-scoped phase, per this project's own stop condition. Phase
1 was a read-only audit (`PHASE_READ_ENGINE_ARCHITECTURE_REPORT.md`,
spec review + full source mapping of the read-relevant code in
`src/lsm/`, `src/sstable/`, `src/memtable/`, `src/manifest/`, no source
touched) — key finding: `LsmEngine::get`/`get_as_of` already implement
the recency-ordered, tombstone-collapsing merge the spec describes;
the real gap is `range_scan`, which doesn't exist yet, and a precise,
traced (not assumed) divergence from the spec's `ReadView` concurrency
model for that specific operation shape.

Phase 2 was `PHASE_READ_ENGINE_ADR.md` (ADR-RE-001), resolving all 13
required decisions (ReadView model, snapshot model/lifetime, `NotFound`
vs. `Ok(None)`, `range_scan` contract, merge ordering, tombstone/
corruption semantics, concurrent-flush visibility, SSTable authority,
`contains()`/`batch_get()` scope, memory ownership, benchmark
instrumentation, final API scope) — again no source touched, verified
via `git status`.

**Implementation Increment 1** (approved scope only: snapshot
infrastructure, `ReadStats` infrastructure, the `ReadView` foundation
type, point-read regression protection, a concurrent-flush point-read
test — explicitly not `range_scan`, not a cache, not `batch_get`, no
change to `get`/`get_as_of` semantics or to `EngineError`):

- `Snapshot`/`SnapshotRegistry` (`src/lsm/mod.rs`): a real, `Drop`-
  released, multiset-correct read-snapshot handle plus
  `oldest_live_snapshot_seq()` — the registration mechanism a future
  Compaction phase will need, built now (not just sketched) since it's
  cheap and independently testable without Compaction existing to
  consume it.
- `ReadStats`/`ReadStatCounters` (`src/lsm/mod.rs`) + two new counters
  on `SsTable` (`src/sstable/reader.rs`: `blocks_read`, incremented
  once inside the shared `read_block` so it will also cover
  `range_scan_raw` for free once `range_scan` lands; `bloom_negative_
  count`, incremented at `get_versioned`'s existing bloom check) —
  `get_as_of` gained exactly these counter increments alongside its
  existing branches/returns, no control-flow or return-value change.
- `ReadView` (`pub(crate)`, foundation only — not yet wired into any
  public method): `Arc`-clones `immutables`/`sstables` and materializes
  only the requested key range from `active`, never full MemTable/
  SSTable contents, never a second Bloom filter or index.
- 17 new tests, all passing, all re-run for stability where timing-
  sensitive: 8 `Snapshot`/`SnapshotRegistry` cases (one/two-different-
  seq/two-same-seq/drop-newest-first/drop-oldest-first/all-dropped/
  seq-stable-after-later-writes), 4 `ReadStats` cases (MemTable hit,
  miss, SSTable hit+consultation+block-reads, bloom-negative miss with
  zero block reads), 2 `ReadView` foundation cases, 2 point-read
  regression cases closing real gaps the architecture report flagged
  (plain overwrite without a delete; data-block corruption detected
  *lazily at read time*, not at `open()` — this one required flipping
  one byte at file offset 0, verified against the writer's actual
  block-then-bloom-then-index-then-footer layout, not guessed), and 1
  concurrent-flush point-read test using the existing `FlushFaultPoint`/
  `install_flush_fault_hook` machinery (no sleeps) to deterministically
  land a lookup inside the exact "SSTable published, immutable not yet
  removed" transition window `ADR-RE-001` §1 traced safe.

**Three real bugs caught and fixed by actually running these tests
before trusting them**, matching this project's own established
discipline: (1) the concurrent-flush test's closure captured an
`mpsc::Receiver` by move, which isn't `Sync` — `install_flush_fault_
hook`'s bound requires it; fixed by wrapping in a `Mutex`. (2) Two
tests used a 150-byte memtable with 10 puts, intended to trigger
exactly one freeze — it actually triggered multiple, so `sstable_
count() == 1`/`immutable_count() == 1` assertions failed; fixed by
calibrating to 350 bytes (37 bytes/entry × 10, same tuning convention
already used elsewhere in this file). (3) The `ReadView` bounded-range
test assumed `Bound::Excluded` on the end key excludes that key's own
entries; it does not, in `MemTable::range`'s existing, already-shipped
`bound_to_tuple` mapping (`Excluded(k)` end bound maps to `Excluded((k,
u64::MAX))`, which every real, sub-`u64::MAX` seq falls under) — a
real, pre-existing discrepancy, explicitly recorded (not silently
worked around) in the test's own comment and flagged for `range_scan`'s
own implementation (the next increment) to explicitly decide how to
handle.

**Full regression gate, re-run and verified clean on the actual
committed state**: `cargo fmt --check`, `cargo clippy --all-targets
--all-features -- -D warnings`, `cargo test --lib` (273/273 = 256
pre-existing + 17 new), `cargo test --release --lib` (273/273),
`cargo check --all-targets --all-features` (examples/benches still
build). No existing Write Engine test's behavior changed.

**Not implemented this increment, deliberately**: `range_scan`, the
k-way merge, `contains()`, `batch_get()` (deferred per the ADR), any
cache, mmap, or benchmark suite. **Not claimed**: Read Engine
production readiness — that requires the full certification matrix
per `ADR-RE-001`'s own §24/§12, none of which has started yet.

**Commit discipline note**: this session's Write Engine certification
work (ADR-WE-SP-001 through the final 16-gate certification) had not
yet been committed when this Read Engine work began, in the same files
this increment also touches. Reconstructed a precise Write-Engine-only
snapshot of `src/lsm/mod.rs`/`src/lsm/tests.rs` (verified it still
builds and passes its own 256/256 tests standalone) to commit that
backlog as its own commit first, then committed this Read Engine
increment separately and focused, per the instruction to keep this
increment's commit scoped to exactly what it implements.

**Open Tier 3 question currently blocking further work:** none for
this increment. Two real, explicitly-recorded discrepancies (`NotFound`
vs. `Ok(None)` already resolved by the ADR; the `MemTable::range`
`Excluded`-end-bound behavior not yet resolved) carry forward into
Increment 2's own scope (`range_scan`, the k-way merge, version
resolution, tombstone handling, range correctness tests) — not started
here, per the instruction to stop and report after this increment.

## 2026-09-20 (continued: Read Engine Increment 2 — production-grade range_scan)

Implemented `LsmEngine::range_scan(start, end, as_of_seq) -> RangeScanIter`
and `range(start, end)` (= `range_scan(.., u64::MAX)`), per `ADR-RE-001`.
Lazy, ordered, bounded-memory, single-logical-value-per-key, built on
Increment 1's `ReadView`/`Snapshot`/`ReadStats` foundation.

**A real, pre-existing bug fixed first, per §6's explicit conditional
authorization** ("if production source is wrong, add the regression
test first, then the smallest correct fix"): `MemTable::range`'s
`bound_to_tuple` used the same sentinel seq for both `Included` and
`Excluded` bounds, so an `Excluded(k)` bound never actually excluded
`k`'s own entries -- a genuine violation of `std::ops::Bound`'s own
unambiguous contract, not intentional behavior, confirmed by two new
regression tests (`range_excluded_start_bound_.../range_excluded_end_
bound_...`) written and shown failing *before* the fix. Fixed by
splitting into `bound_to_tuple_start`/`bound_to_tuple_end`, each
choosing the sentinel that actually enforces exclusion in its own
direction. All 17 memtable tests (including the property tests)
re-verified clean afterward. This is the one deliberate, pre-authorized
exception to "do not touch protected Write Engine files" this
increment made -- flagged explicitly, not silently done.

**Algorithm** (`RangeScanIter`, `src/lsm/mod.rs`): a real binary-heap
k-way merge (`BinaryHeap<Reverse<HeapEntry>>`, ordered by `(key asc,
source recency asc)`, matching the LSM Engine Spec §4.2's own
description almost verbatim once re-read directly). Because both
`MemTable::range` and `SsTable::range_scan_raw` return iterators that
*borrow* from the `MemTable`/`SsTable` they're called on, and
`RangeScanIter` needs to *own* the `Arc<MemTable>`/`Arc<SsTable>` it
reads from (a self-referential-struct shape Rust can't express without
`unsafe` or an external crate), each source instead tracks only its own
resume point (an owned `Bound<Vec<u8>>`) and makes one fresh, short-lived
call per distinct key -- reusing `range`/`range_scan_raw` exactly as
they exist today (`ADR-RE-001` §3's explicit instruction), never
rewriting their iteration behavior. **Known, deliberately deferred
performance characteristic, documented in code and here rather than
hidden**: this can re-run an SSTable's block-locating search and
re-read+re-decode a block once per distinct key it holds, rather than
once per block -- correctness is unaffected (every block read still
goes through the one shared, already-instrumented `read_block`), and
this is explicitly left for the future benchmark/optimization phase to
measure and address, per the ADR's own "do not optimize prematurely."

**Version resolution / tombstones**: within each source, the highest
`seq <= as_of_seq` wins; among sources sharing a key, the first
(newest, by recency) source with *any* visible version wins outright --
proven structurally equivalent to "highest seq across all sources" (the
spec's own phrasing) under this project's existing recency-ordering
invariant, not a new, separate rule. A winning tombstone suppresses the
key entirely -- never yielded, never a sentinel, never falls through to
an older source's value.

**Corruption**: fail-closed, matching `range_scan_raw`'s own existing
contract exactly -- first `Err` ends the whole iterator immediately, no
skipped table, no partial success. **Caught and fixed a real bug in my
own first draft before it shipped**: the initial version of `peek_
sstable`'s multi-version-collection loop silently `break`-ed out on a
mid-group `Err`, returning the already-collected (incomplete) versions
as if they were a complete success and advancing the resume point past
the corrupted record -- exactly the silent-corruption-skip the ADR
forbids. Fixed to propagate the `Err` immediately instead.

**Bounds**: a second real bug caught by running the new empty/edge-case
test before trusting it: `std::collections::BTreeMap::range` *panics*
(not "returns empty") on `start > end` or `Excluded(x)..Excluded(x)` --
both mathematically empty intervals. Added `range_is_definitely_empty`,
checked before ever constructing a `BTreeMap`-backed range, so these
cases return a genuinely empty `RangeScanIter` instead of panicking.

**Snapshot**: `range_scan(.., snapshot.seq())` observes exactly the
historical state as of that snapshot, unaffected by later writes;
dropping it correctly releases the registration (`oldest_live_
snapshot_seq()` reflects it at every step) -- reusing Increment 1's
`Snapshot`/`SnapshotRegistry` exactly, no changes needed there.

**Concurrent flush**: a new deterministic test (`FlushFaultPoint`, no
sleeps -- same established mechanism as Increment 1's point-lookup
concurrency test) captures a `range_scan`'s `ReadView` while a flush is
deliberately stuck between publishing an SSTable and removing the
corresponding immutable, then verifies the scan's output has no
duplicate keys, no missing pre-existing keys, and no impossible key --
coherent regardless of which side of the transition the capture landed
on.

**`ReadStats`**: `read_requests` increments once per `range_scan`/
`range` *call* (never once per row, per §13's explicit instruction);
`read_hits` increments once per yielded row (a documented, explicit
choice, since a range scan has no single hit/miss outcome the way a
point lookup does); `sstables_consulted` increments once per real
physical query against a table; `blocks_read` was already free
(Increment 1's shared `SsTable` counter, inside `read_block`, hit by
both `get_versioned` and `range_scan_raw` identically).

**Tests, all newly added and all passing** (matching the phase brief's
own 17-item matrix): basic scan; included/excluded/unbounded bounds;
the full empty/edge-case matrix (empty DB, no-match range, start>end,
degenerate Excluded==Excluded, single-key range, every Included/
Excluded/Unbounded combination); a range spanning 5+ live SSTables;
active+multiple-immutables+SSTable merged together; multiple versions;
tombstone suppression; delete/recreate; the ADR's own three worked
version-resolution examples reproduced against real, separately-flushed
SSTables; snapshot range + registry correctness; `get`/`range_scan`
equivalence (every key, every seq actually produced); concurrent-flush
coherence; corrupted-data-block fail-closed-and-ends; a deterministic
differential test against an independent (non-production-algorithm)
reference model spanning active+immutable+SSTable; a 64-case
`proptest`-based property test (matching this project's own established
I/O-heavy-test case-count convention) generating random PUT/DELETE
sequences and checking ordering, no-duplicate-keys, and full
reference-model equivalence; `ReadStats` correctness for the range
path.

**Full regression gate, run and verified clean on the actual code
about to be committed**: `cargo fmt --check`, `cargo clippy
--all-targets --all-features -- -D warnings`, `cargo test --lib`
(293/293 -- 273 at the end of Increment 1, plus 20 new tests this
increment: 2 `MemTable` `Excluded`-bound regression tests plus 18 new
`LsmEngine`-level tests covering `range_scan`, one of which drives 64
randomized property-test cases internally), `cargo test --release
--lib` (293/293, confirmed clean across 7 consecutive full-suite runs
after investigating one single intermittent failure -- traced to the
same pre-existing, already-documented `coordinator_panic_before_batch_
formation_fails_safely` flake noted in earlier entries, in a file none
of this increment's work touches; the two new concurrency-sensitive
tests this increment added were separately stress-run 6/6 clean each),
`cargo check --all-targets --all-features`, plus `wal_tests` (12/12),
`crash_consistency --features test-util` (2/2),
`pathological_recovery_matrix` debug+release (9/9 each), and the
storage-pressure test re-verified fresh (5/5). No existing Write Engine
test's behavior changed. A small sanity comparison (100-writer full-
pipeline write throughput, 3 reps: 17,157 / 11,706 / 16,098 ops/sec)
stayed within this project's own already-documented historical
variance band -- not a rigorous benchmark (that's explicitly deferred
to the next phase per the ADR's own §19/§24), just confirmation nothing
is obviously, grossly disturbed.

**Diff scope**: `src/lsm/mod.rs`, `src/lsm/tests.rs` (primary), and
`src/memtable/mod.rs` (the one pre-authorized bug fix above). No
changes to `src/wal/`, `src/manifest/`, `src/execution/`, or
`src/error.rs`.

**Not implemented this increment, deliberately**: the performance
benchmark suite, any cache/prefetch/mmap/parallel execution, `batch_get`,
the full corruption matrix (one corruption case was added; a complete
matrix mirroring the Write Engine's own is future work), long-duration
read soak, and — explicitly — **Read Engine production-readiness
certification**. `PHASE_READ_ENGINE_CERTIFICATION.md` does not exist
yet and should not be inferred from this increment's passing tests
alone.

**Status: READ ENGINE NOT READY** (not a regression -- it was never
claimed ready; `range_scan` now exists and is well-tested, which is
real, meaningful progress, not a certification).

**Open Tier 3 question currently blocking further work:** none. The
next increment's own scope (per the phase brief's §21 remaining items
and `ADR-RE-001`'s §24 certification gates) is the performance
benchmark suite, the full corruption matrix, `contains()`, the
long-duration read soak, and integrated write+read testing -- not
started here, per the instruction to stop and report after this
increment.

## 2026-09-20 (continued: Read Engine Increment 3 -- performance + observability + contains())

`LsmEngine::contains(key, as_of_seq) -> Result<bool>` (`ADR-RE-001`
§2/§10): mirrors `get_as_of`'s exact active → immutables → SSTables
recency-ordered traversal line for line (not implemented as a wrapper
around it, so the invariant `contains(k,s) == get_as_of(k,s)?.is_some()`
is exercised as a real test, not assumed from shared code), backed by
a new `SsTable::contains_versioned` that reuses the identical bloom +
sparse-index + candidate-block walk as `get_versioned` without ever
constructing an owned `RecordValue`. 11 new tests (304/304 total):
hit/miss, visible tombstone, delete-then-recreate, `as_of_seq`
filtering across multiple versions, active+immutable+SSTable overlap,
multi-SSTable consultation, two `ReadStats` wiring tests, and -- the
matrix's real gap-closer -- two genuine (not simulated) I/O-failure
tests for `contains`/`get_as_of`/`range_scan`, built by shrinking a
live SSTable's file out from under its already-open, already-validated
`SsTable` so `read_block` hits a real `io::ErrorKind::UnexpectedEof`,
asserted as `Err(EngineError::Io(_))` specifically (not `is_err()`).
The differential reference-model test and the 64-case property test
were both extended in place to check the same three-way invariant
(reference model == `get_as_of` == `contains`) at every seq boundary,
not just added as separate tests.

**`examples/read_engine_bench.rs`** (new, ~800 lines): a real,
unmocked measurement harness -- every fixture is a real `LsmEngine`
populated through the real write path (real WAL group-commit, real
background flush thread, real `.sst` files), never a mock. Ten
sections (`point_lookup`, `tombstone`, `contains_vs_get`, `read_amp`,
`range`, `memory`, `fd`, `concurrency`, `sanity`,
`readstats_overhead`), each independently runnable. Full run, every
number transcribed (not summarized from a best/median-only view) into
the new `PHASE_READ_ENGINE_PERFORMANCE.md`. Headline findings:

- **`contains()` vs `get_as_of(..).is_some()`: no measurable
  performance difference**, at any value size tested (32B/1KB/16KB) or
  SSTable count (10/1000) -- the honest verdict the brief explicitly
  demanded ("do not keep `contains()` just because the ADR predicted a
  benefit"), traced to why: `read_block`/`decode_block` already
  eagerly decodes every record's value bytes for a whole block
  regardless of which method reads it, so the value-copy `contains()`
  was meant to avoid was already free (a `Vec` move, not a clone) in
  `get_versioned`'s own existing code.
- **Read amplification scales roughly linearly with live SSTable
  count** for point lookups (a hit at ~1333 live SSTables consults
  ~1167 of them on average before finding a match, since there is no
  Compaction yet to bound the table count) -- but almost entirely via
  cheap bloom-negatives, not disk I/O (`avg_blocks_read` stays at
  ~27 even when ~1167 tables were consulted). The clear, measured
  bottleneck at scale is CPU-bound bloom-filter checking, not I/O --
  exactly what a future Compaction phase would fix. Not optimized
  here, per the brief's explicit instruction; only identified.
- **`range_scan`'s bounded-memory design verified with a real number,
  not just an architectural claim**: a full scan over a 25.0 MiB,
  200-SSTable, 16KB-value dataset grew peak RSS by only 168KB.
- **A real, rare, and fully explained cross-thread finding**: a reader
  thread sampling `snapshot_seq()` from *another* thread's writes and
  then reading at that pinned seq can, extremely rarely (~1 per 1.4M
  checks), transiently disagree with a repeat read at the same seq.
  Reproduced with plain `get_as_of` alone (no `contains()` involved),
  and traced directly in the source -- `freeze_locked` and the flush
  thread's SSTable-publish-before-immutable-removal ordering are both
  race-free by inspection -- leaving `snapshot_seq()`'s own documented
  same-thread-safe caveat (`durable_through` reflects WAL durability,
  not "every other thread's `apply_after_durable` has completed") as
  the explanation. Verified as same-thread-safe (writer thread checking
  its own just-completed write: zero disagreements across 664 writes
  in the same run). Not a `contains()`/Increment-3 bug, not a protected
  Write Engine change made or needed here -- flagged for visibility,
  not silently resolved.
- File-descriptor/thread counts confirmed stable across 2000 point
  lookups + 20 full range scans at ~1333 live SSTables (handle delta:
  0) -- no per-read handle leak.
- `ReadStats` instrumentation overhead bounded at ~4.5ns/counter
  increment (a best-effort proxy measurement; no A/B toggle exists or
  was added, since one would touch the production read path beyond
  this increment's approved scope) -- negligible against the
  microsecond-scale latencies measured everywhere else in this run.

**No optimization was added this increment** -- no cache, mmap,
prefetch, parallel reads, secondary index, or read worker pool. Both
headline findings above (read-amp scaling, no `contains()` win) are
exactly the "measure first" evidence the brief required before any of
those could even be considered, and per its explicit instruction,
acting on them needs a new ADR, not a unilateral change here.

**Corruption matrix** (`PHASE_READ_ENGINE_PERFORMANCE.md`'s own table):
every read path (`get`/`get_as_of`, `range`/`range_scan`, `contains`)
now has both a checksum/structural-corruption test and a genuine
(not simulated) I/O-failure test, each asserting the exact
`EngineError` variant. Plus the pre-existing open-time/manifest/
missing-file/orphan-file corruption tests, unchanged.

**Full regression gate, run and verified clean on the actual code
committed**: `cargo fmt --check`, `cargo clippy --all-targets
--all-features -- -D warnings`, `cargo test --lib` (304/304 -- 293 at
the end of Increment 2, plus 11 new tests this increment), `cargo test
--release --lib` (304/304), `cargo check --all-targets --all-features`,
`wal_tests` (12/12), `crash_consistency --features test-util` (2/2),
`pathological_recovery_matrix` debug+release (9/9 each), and the
storage-pressure state-machine test re-verified as part of the full
304-test `--lib` run. No existing Write Engine or Read Engine test's
behavior changed.

**Diff scope**: `src/lsm/mod.rs` (+48, `contains()`),
`src/sstable/reader.rs` (+52, `contains_versioned`), `src/lsm/tests.rs`
(+458, new/extended tests), `examples/read_engine_bench.rs` (new),
`PHASE_READ_ENGINE_PERFORMANCE.md` (new). Every change to existing
files is a pure addition (no deletions, no edits to existing lines
outside the two files' own already-reviewed diffs). No changes to
`src/wal/`, `src/manifest/`, `src/execution/`, or `src/error.rs`.

**Not implemented this increment, deliberately**: any caching/mmap/
prefetch/parallel-read/secondary-index optimization, `batch_get`, the
long-duration read soak, integrated write+read endurance testing
(the 5-second bounded sanity workload is not a soak), and --
explicitly -- **Read Engine production-readiness certification**.
`PHASE_READ_ENGINE_CERTIFICATION.md` does not exist yet.

**Status: READ ENGINE NOT READY** (not a regression -- `contains()`
and a real performance baseline now exist and are well-tested, which
is real, meaningful progress, not a certification).

**Open Tier 3 question currently blocking further work:** none, except
the cross-thread `snapshot_seq()` finding above, which is flagged for a
future, narrowly-scoped Write Engine investigation if cross-thread
linearizable snapshot reads become a requirement -- not blocking this
increment's own completion, and not something this increment is
authorized to change. The next increment's own scope (per `ADR-RE-001`
§24's remaining certification gates) is the long-duration read soak,
integrated write+read endurance testing, any justified optimization
(only if backed by a new ADR), and the final certification matrix --
not started here, per the instruction to stop and report after this
increment.

## 2026-09-20 (continued: Read Engine Increment 4 -- long-duration read soak + integrated write/read endurance)

A real, 4-hour (`duration_secs=14400`), production-profile soak
(`examples/read_write_soak_test.rs`, new -- mirrors this project's own
established `realistic_full_pipeline_soak.rs` endurance methodology
rather than a bounded smoke test) exercising the full real stack (WAL,
MemTable, immutable MemTables, SSTables, Manifest, checkpoint, WAL
purge) under 8 concurrent writers + 16 concurrent readers, continuously
validated against an independent reference model (never the production
merge algorithm itself as the oracle -- brief's own explicit
instruction). `RESULT=PASS`: `writes_issued=13,483,811
deletes_issued=3,375,298 reads_issued=4,582,352
range_scans_issued=261,455 in_run_mismatches=0 recovery_ok=true
post_recovery_mismatches=0 capacity_backpressure_events=0
final_sstables=614 final_db_bytes=2,368,131,525
final_rss_kb=2,011,784`. `storage_state` stayed `Healthy` throughout.
Also extended in this increment: `corruption_injected_mid_session_
between_reads_is_caught_on_the_very_next_read` (`src/lsm/tests.rs`,
new -- corruption injected while the engine stays open and has already
served a successful read from the exact table, no restart, closing a
gap the restart-based corruption tests didn't cover) and
`examples/lsm_crash_cycle_test.rs`'s own verification extended from
"open() returned Ok" to real `get`/`contains`/`range` reads against an
exact expected value after each crash+recovery cycle.

**Status: READ ENGINE NOT READY.** The soak's own `RESULT=PASS` is a
real, meaningful correctness/endurance result -- not itself a
certification. Flagged, not silently resolved: `snapshots_live=50`
stayed constant throughout the run and `rss_kb` grew to ~2GB by the
end, and `range_large` latency was visibly high late in the run --
none of these were treated as automatic failures (none violate any
stated acceptance criterion) but all three were carried forward as the
explicit trigger for a dedicated Increment 5 investigation rather than
assumed benign or silently optimized.

## 2026-09-20 (continued: Read Engine Increment 5 -- memory + range-performance investigation)

Investigated the three items flagged above, against Increment 4's
completed soak log (`temp/read_write_soak_output.log`, preserved
unmodified as historical evidence, not rerun). Full detail: `PHASE_
READ_ENGINE_RESOURCE_INVESTIGATION.md` (new).

**Range-scan latency: root cause found, traced in source, and
independently reproduced.** `range_large` p50 grew from 1.15ms (5
SSTables) to 43.1 **seconds** (597 SSTables) -- super-linear (~n^2.2
apparent exponent), unlike point lookups' already-known linear
scaling. Traced to `RangeScanIter::refill` (`src/lsm/mod.rs:706-733`)
re-peeking every source holding a version of each winning key, which
on this soak's own small-cardinality (`KEY_CARDINALITY=4000`), heavily
-overwritten (~27,424 writes/flush-cycle) keyspace means ~99.9% of
keys land in nearly every live table -- reducing the cost to
O(distinct keys yielded × live SSTable count). **Reproduced exactly**
in a new, deterministic ~3-minute benchmark (`examples/read_engine_
bench.rs`'s new `overlap_repro` section): `sstables_consulted/sstable`
pinned at a constant integer (21.000) across five SSTable-count
checkpoints once the workload's overlap ratio matched the soak's own
regime -- a first, lower-overlap attempt (reported, not hidden) showed
only 1.6-2.5x, itself evidence that overlap ratio, not raw SSTable
count, is the driving variable. `PHASE_READ_ENGINE_RANGE_PERFORMANCE_
ADR.md` (new, ADR-RE-002) evaluates four fix options (persistent
source cursors via an owned-`Arc` iterator refactor, block-position
reuse, range-aware seeking, reduced bloom/index work) and proposes a
direction (persistent source cursors) for a *future* increment's
decision -- **no optimization implemented this increment**, per the
brief's explicit instruction; no cache/mmap/prefetch/parallel-read
evaluated at all (explicitly out of scope without their own dedicated
ADR).

**Memory: no leak found.** RSS's monotonic growth component is fully
explained, with source evidence (no static cache anywhere in the read
path, no engine-side registry of live range scans -- both confirmed by
`grep`, not assumed), by per-SSTable `BloomFilter`/`Vec<IndexEntry>`
metadata that is expected to persist until a future Compaction phase
exists (none does yet) -- consistent in shape with, and now explaining
the absolute-magnitude gap against, Increment 4's own `memory_scaling`
section's much smaller tiny-fixture numbers. Non-monotonic
multi-hundred-MB RSS swings (worst: -704MB across two samples) are not
explained by any code-level structure (nothing in this engine shrinks
pre-Compaction) and are most plausibly, though not independently
profiler-confirmed here (no such tool available in this environment),
attributed to Windows working-set volatility. `snapshots_live=50`
staying constant for all 115 samples was verified, by source review of
`SnapshotRegistry` (`src/lsm/mod.rs:266-342`, a correctly refcounted
`Mutex<BTreeMap<seq, count>>`), to be the *test harness's own*
deliberate 50-snapshot pool cap (`SnapshotPool::prune(50)` in
`read_write_soak_test.rs`), not an engine-side leak -- no snapshot
semantics were changed. File handles track SSTable count 1:1 (~1.00
handle/table across the run), confirming Increment 3 §8's finding
still holds at production scale under real concurrent load across
261,455 range scans; thread count stays flat during the run and shuts
down cleanly after.

**Certification status, not collapsed into PASS/FAIL** (per the
brief's own explicit instruction): correctness **PASS** (unchanged),
performance **OPEN** (range-scan finding, not yet addressed), memory
**OPEN but no leak found** (residual uncertainty about the OS-level
RSS swings specifically). **Status: READ ENGINE NOT READY.**

**Diff scope this increment**: `examples/read_engine_bench.rs`
(+`section_overlap_repro`, new benchmark section), `PHASE_READ_ENGINE_
RESOURCE_INVESTIGATION.md` (new), `PHASE_READ_ENGINE_RANGE_
PERFORMANCE_ADR.md` (new), `PHASE_READ_ENGINE_PERFORMANCE.md` (+one
summary section), this file. No production source file
(`src/lsm/mod.rs`, `src/sstable/`, `src/wal/`, `src/manifest/`)
changed -- per the brief's explicit instruction, this increment
investigates and reports; it does not optimize.

**Next increment's scope (not started here)**: a maintainer decision
on `ADR-RE-002`'s proposed direction, followed by (if approved) an
implementation increment for Option A (persistent source cursors),
re-running the full corruption matrix and a before/after
`overlap_repro`-style benchmark before claiming any improvement.

## 2026-09-20 (continued: Read Engine Increment 6 -- `ADR-RE-002` Option A implemented: persistent source cursors)

Implemented exactly the direction `ADR-RE-002` proposed and nothing
else -- no cache, no mmap, no prefetch, no parallel-read workers, no
secondary index, no Compaction/Router/Replication, no Write Engine/
WAL/Manifest change.

**Implementation.** New `SsTableRangeCursor` (`src/sstable/reader.rs`,
126 lines added, zero removed -- `RangeScanRaw`/`range_scan_raw` left
completely unmodified, still exactly what they were before this
increment): functionally identical to `RangeScanRaw` (same start-block
binary search, same lazy one-block-at-a-time reads via the shared
`read_block`, same bounds/corruption-propagation contract) but owns
its own `Arc<SsTable>` clone and owned `Bound<Vec<u8>>` bounds instead
of borrowing `&'a SsTable`/`Bound<&'a [u8]>` -- which is what makes it
safe to store *inside* `RangeScanIter` for a scan's whole lifetime
without a self-referential struct: it borrows nothing from the struct
holding it, only from its own owned `Arc` clone (a refcount bump, the
same pattern `ReadView` already used). `RangeScanIter`
(`src/lsm/mod.rs`) now holds `sstable_cursors: Vec<Option<Peekable
<SsTableRangeCursor>>>` -- one persistent cursor per live SSTable
source, constructed once in `new` and driven forward with ordinary
`Peekable::peek`/`next` for the scan's entire remaining lifetime,
replacing the old `sstable_next_start: Vec<Option<Bound<Vec<u8>>>>`
resume-point-plus-fresh-call design that forced a binary search and a
discarded/re-read block on every single key drawn from a source. The
k-way merge algorithm itself (`refill`/`Iterator::next`) is unchanged
-- only `peek_sstable`'s body changed (now pulls from the persistent
cursor instead of constructing a fresh one). `MemTable`/immutable
sources deliberately left untouched (`src/memtable/mod.rs` not
touched at all) -- the measured bottleneck was entirely SSTable-side
(`PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md` §4), and `MemTable::
range` is a cheap in-memory `BTreeMap::range` call, never the
re-read-a-block-from-disk cost the SSTable side had. **Zero `unsafe`,
zero new dependency** (`Cargo.toml`/`Cargo.lock` unchanged, confirmed
by `git status`) -- proving the approved design (owned-`Arc` refactor,
no `ouroboros`/`self_cell`) was sufficient, exactly as `ADR-RE-002` §3
Option A predicted.

**`ReadStats` semantics, intentionally revised and documented, not
silently changed (brief §16)**: `sstables_consulted` for range scans
now increments once per live SSTable actually captured by a scan's
`ReadView` (matching point lookups' own "once per table checked...
whether or not that check was a bloom-negative" convention) instead of
once per distinct key drawn from a source (the old design's necessary
side effect of every key draw being a fresh `range_scan_raw` call).
`blocks_read`'s counting point (`SsTable::read_block`) is completely
unchanged. New regression test, brief §23's explicit ask ("prefer an
observable test hook/counter over timing"): `lsm::tests::range_scan_
source_cursor_persists_across_keys_instead_of_reconstructing_per_key`
(`src/lsm/tests.rs`) builds a small, deliberately overlapping keyspace
(5+ SSTables each holding a version of nearly every one of 15 keys)
and asserts `sstables_consulted` increases by *exactly* the live
SSTable count for one range scan -- not merely more than before, which
the old design would also have satisfied; a regression reintroducing
per-key reconstruction fails this assertion immediately, independent
of timing.

**Benchmark evidence -- before/after, same unmodified `overlap_repro`
workload, `n=7` reps/checkpoint (full table: `PHASE_READ_ENGINE_
PERFORMANCE.md`'s dated Increment 6 section)**. Old numbers captured
by `git stash`-ing just the implementation files (`src/lsm/mod.rs`,
`src/sstable/reader.rs`, `src/sstable/mod.rs`) back to their
pre-Increment-6 committed state, rebuilding, rerunning the identical
benchmark invocation, then restoring and rerunning unchanged -- same
machine, same `--release` build, same dataset generation, same PRNG
seed, same checkpoints (20/50/100/200/300 SSTables), same range bound.

| SSTables | OLD p50 (us) | NEW p50 (us) | speedup | OLD blocks_read | NEW blocks_read |
|---:|---:|---:|---:|---:|---:|
| 20  | 5,169.9  | 1,617.5  | 3.20x | 660   | 140   |
| 50  | 14,251.1 | 3,862.5  | 3.69x | 1,650 | 350   |
| 100 | 27,451.3 | 8,006.1  | 3.43x | 3,300 | 700   |
| 200 | 56,331.6 | 16,947.4 | 3.32x | 6,600 | 1,400 |
| 300 | 90,469.2 | 24,723.0 | 3.66x | 9,900 | 2,100 |

`blocks_read` (unchanged counting point) dropped by an exact, constant
**4.714x** at every checkpoint -- direct, apples-to-apples proof of the
mechanism fix, not just "faster." Wall-clock p50 improved
**3.20x-3.69x** across all five checkpoints. `sstables_consulted`
dropped by an exact 21x at every checkpoint (reflects both the
mechanism fix and the documented counter-definition change together,
not a clean isolated number -- `blocks_read` is the number to cite for
the mechanism alone).

**Resource-lifetime check** (`read_engine_bench cursor_resource_check`,
new section): 200 create/partial-consume/drop + 200 create/full-
consume/drop cycles (400 scans) against a 5-SSTable overlapping
fixture -- handle delta = 0, thread delta = 0, RSS delta = 220 KB
total across all 400 scans (~0.55 KB/scan, ordinary allocator noise,
not a leak). Point-lookup regression check: `point_p50_us` at every
checkpoint stayed within this benchmark's own single-digit-microsecond
noise floor old vs. new (expected -- `get`/`get_as_of`/`contains` and
`SsTable::get_versioned`/`contains_versioned` were not touched;
confirmed by diff, `src/sstable/reader.rs`'s entire diff is additive).

**Full regression suite, run and verified clean**: `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test
--lib` (306/306 -- 305 at the end of Increment 4, plus this
increment's one new regression test), `cargo test --release --lib`
(306/306), `cargo check --all-targets --all-features`, `wal_tests`
(12/12), `crash_consistency --features test-util` (2/2),
`pathological_recovery_matrix` debug+release (9/9 each). The full
`--lib` run already covers every named corruption/range-bounds/
version-tombstone/snapshot/concurrent-flush/property test (`lsm::
tests::range_scan_during_concurrent_flush_sees_a_coherent_snapshot`,
`range_scan_across_a_corrupted_data_block_fails_closed_and_ends`,
`range_scan_included_bounds`/`excluded_bounds`/`unbounded_start_or_
end`, `range_scan_property_tests::lsm_engine_range_scan_matches_
independent_reference_model`, and every other `range_scan_*`/
`*snapshot*` test individually confirmed still passing by name). No
existing test's assertions were altered.

**Diff scope**: `src/sstable/reader.rs` (+126/-0, `SsTableRangeCursor`
+ `SsTable::range_scan_cursor`), `src/sstable/mod.rs` (+1/-1, export),
`src/lsm/mod.rs` (+126/-53, `RangeScanIter` refactor + doc comments +
revised `sstables_consulted` counting point), `src/lsm/tests.rs` (new
regression test -- this file's diff also still carries Increment 4's
own not-yet-committed `corruption_injected_mid_session_between_reads_
is_caught_on_the_very_next_read` test, bundled into this commit as a
side effect of sharing the file, not itself Increment 6 work),
`examples/read_engine_bench.rs` (new `cursor_resource_check` section,
`overlap_repro` enhanced to `n=7` reps/checkpoint with p50/p95/p99/max
-- this file's diff also still carries Increment 4's `memory_scaling`
section and Increment 5's `overlap_repro` section, neither committed
until now, both swept in as the same side effect), `PHASE_READ_ENGINE_
PERFORMANCE.md`/`PHASE_READ_ENGINE_RANGE_PERFORMANCE_ADR.md` (dated
Increment 6 sections, `ADR-RE-002` status flipped to Implemented),
this file. **Not** included, deliberately out of this increment's
scope: `examples/lsm_crash_cycle_test.rs`, `examples/read_write_soak_
test.rs` (both pure Increment 4 work, unrelated to range-scan cursors).
No `src/wal/`, `src/manifest/`, `src/error.rs`, `Cargo.toml`, or
`Cargo.lock` change.

**`ADR-RE-002`: IMPLEMENTED.** Correctness unregressed, no protected
behavior changed, no new resource leak, measured and reproducible
improvement on the exact workload that demonstrated the original
problem. **Status: READ ENGINE PRODUCTION READY = NO** -- final
corruption/recovery validation, final integrated endurance validation,
final performance validation, and the final certification matrix
remain outstanding, unstarted gates. No Compaction, Router, or
Replication work started. No new soak run.

## 2026-09-21 (Read Engine Increment 7 -- fresh 4-hour integrated soak against the optimized implementation)

Closed the one gap Increment 6 left explicitly open: re-validated the
`ADR-RE-002` Option A optimization against a fresh, full 4-hour,
production-profile integrated write/read soak -- not just the
controlled `overlap_repro` benchmark. Full detail: `PHASE_READ_ENGINE_
INCREMENT7_SOAK.md` (new).

**Precondition check surfaced one unexpected commit** (`3e13f64`,
"commit by me", authored outside this session, sitting on top of the
expected `22be3e4` HEAD) -- inspected before proceeding: touches only
`examples/lsm_crash_cycle_test.rs` and adds `examples/read_write_soak_
test.rs`, zero `src/`/`Cargo.toml`/`Cargo.lock` changes. Recognized as
the same Increment 4 test/example content already reviewed in
Increment 5/6 (and the exact harness this soak needed) -- flagged, not
silently proceeded past, judged safe since it carries no production
code change. Full pre-soak gate (`fmt`/`clippy`/`test --release --lib`
306/306/`check`) re-run and clean before starting.

**Soak**: identical profile to Increment 4's own
(`duration_secs=14400 writer_count=8 reader_count=16 seed=20260920
sample_interval_secs=120`), fresh unique directory (`E:\RubiXDb\temp\
read_write_soak_increment7_20260920_213353` -- Increment 4's own
directory was already removed by its own on-PASS cleanup, not reused).
`RESULT=PASS`: `writes_issued=6,696,650 deletes_issued=1,675,511
reads_issued=6,116,654 range_scans_issued=678,708 in_run_
mismatches=0 recovery_ok=true post_recovery_mismatches=0 capacity_
backpressure_events=0 final_sstables=305 final_rss_kb=1,712,740`.
Every one of the 678,708 range scans issued was checked against the
independent reference model at an aged snapshot seq; zero disagreed.
Zero `MISMATCH`/`panic`/`ABORT`/`StoragePressure`/`StorageFull` lines
anywhere in the full 1,054-line log; all 116/116 `HEALTH` samples show
`storage_state=Healthy`.

**A real, expected difference from Increment 4, stated plainly**: this
soak issued fewer total writes (8.37M vs 16.86M) but ~2.6x more range
scans (678,708 vs 261,455) in the same 4 hours on the same 8-core
machine -- the direct, mechanical consequence of range scans no longer
burning CPU on redundant re-peeks: reader threads complete more real
range operations per second, leaving writers a smaller share of the
same fixed CPU budget. Evidence the fix is real under production
concurrent load, not a benchmark artifact.

**Range-scan latency vs. Increment 4, matched SSTable counts (not
cherry-picked -- every point uses a count equal to or higher for
Increment 7)**: `range_large` p50 improved **3.26x-3.89x** across four
matched checkpoints (~58-289 SSTables) -- landing inside the
3.20x-3.69x Increment 6's own controlled benchmark predicted. Growth
curve itself flatter (~1.25 apparent exponent vs. Increment 4's own
~2.20). `blocks_read`-based amplification (counting point unchanged)
improved 5.73x-8.05x at matched counts.

**Resource behavior improved measurably, not just held steady**: RSS-
vs-SSTable-count linear fit tightened from R²=0.698 (Increment 4) to
**R²=0.984**; only 1 of 115 sample transitions showed any RSS decrease
at all (vs. Increment 4's largest single-window drop of −704,440 KB).
Consistent with, not proven to cause, the hypothesis that Increment 4's
own long `range_large` stalls were entangled with OS-level working-set
volatility. `snapshots_live` stayed at exactly 50 for all 116 samples
(harness's own pool cap, unchanged finding). Handles tracked SSTable
count at ~0.99/table; threads stable 29-31, dropped to 4 on shutdown --
no leak.

**Crash/recovery** (bounded, run separately from the primary soak, its
own directory, per the brief's own instruction): `lsm_crash_cycle_test
20 6 20260920 100 1500` -- 20/20 cycles successful, every cycle
`reads_verified_ok=true` (real post-recovery `get`/`contains`/`range`
checks against exact expected values, not merely "open() succeeded"),
`total_read_mismatches=0`.

**Final regression gate, re-run and clean**: `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test
--lib` (306/306), `cargo test --release --lib` (306/306), `cargo check
--all-targets --all-features`, `wal_tests` (12/12), `crash_consistency
--features test-util` (2/2), `pathological_recovery_matrix` debug+
release (9/9 each). No test assertion altered.

**Increment 7 = PASS.** All success criteria met: duration reached
14,400s, zero mismatches/crashes/deadlocks/corruption/leaks, optimized
range scan shows the expected, matched-count-verified improvement with
no new regression. **Status: READ ENGINE PRODUCTION READY = NO** --
final evidence consolidation, final performance validation, final
resource validation, and `PHASE_READ_ENGINE_CERTIFICATION.md` (does
not exist yet) remain outstanding. No Compaction, Router, or
Replication work started. No further optimization performed.

## 2026-09-21 (Read Engine: final certification)

`PHASE_READ_ENGINE_CERTIFICATION.md` (new) — the final certification
document for the single-engine, non-partitioned LSM Read Engine,
certifying commit `22be3e4`. Does not re-derive evidence; every claim
references the historical document/test/commit that actually produced
it. Full 30-row PASS/FAIL/OPEN certification matrix (point lookup,
`get_as_of`, `range_scan`, bounds, version resolution, tombstones,
snapshots, snapshot registration, concurrent-flush visibility,
SSTable-visibility authority, corruption handling, I/O-error
propagation, fail-closed iteration, `contains`, `ReadStats`, memory,
file handles, threads, range-scan performance, point-read performance,
read amplification, crash safety, recovery, integrated workload,
long-duration stability, storage behavior, regression suite, code
quality, dependency hygiene, protected Write Engine integrity):
**30/30 PASS, 0 FAIL, 0 mandatory OPEN**. Rows 15 (`ReadStats`
semantic change), 16 (memory -- no profiler confirmation available),
19/21 (performance/read-amplification -- scoped to the tested
overlapping-key workload, Compaction still absent) carry explicitly
documented, non-blocking caveats.

**Precondition audit**: the unexpected `3e13f64` commit
(`examples/lsm_crash_cycle_test.rs` + new `examples/read_write_soak_
test.rs`, zero `src/` changes) re-characterized explicitly as test/
example-only, not silently ignored. **Protected Write Engine audit**:
`git diff 7d02554 HEAD --stat -- src/wal/ src/manifest/ src/error.rs
src/execution/batch_coordinator/` produced zero output across the
entire Read Engine phase (Increments 1-7 combined) -- WAL, Group
Commit, Batch Coordinator, Manifest, checkpoint, WAL purge, and
`StoragePressure`/`StorageFull` logic confirmed byte-for-byte
unchanged since the Write Engine's own certification. **Final
regression gate re-run clean** (`fmt`/`clippy`/`test --lib` 306/306
debug+release/`check`/`wal_tests` 12/12/`crash_consistency` 2/2/
`pathological_recovery_matrix` 9/9x2). **Bounded final performance
validation** (not a new soak): re-ran `overlap_repro` once at the
certified commit -- `blocks_read`/`sstables_consulted` exactly
reproduced Increment 6's recorded numbers (deterministic), p50 within
ordinary run-to-run noise.

# READ ENGINE PRODUCTION READY = YES

Scope: the RubiXDB single-engine, non-partitioned LSM Read Engine only.
Compaction, Router, Replication, and the larger partitioned RubiXDB
architecture are explicitly **not** certified and do not exist in this
codebase yet. Full RubiXDB production readiness is not claimed.

## 2026-09-21 (Compaction Increment 1: deterministic core implementation)

`ADR-COMPACTION-001` implemented as the deterministic core operation
only -- no automatic trigger, no background thread, by explicit design
(Decision 14). Full detail: `PHASE_COMPACTION_INCREMENT1_RESULTS.md`.

**Delivered**: `LsmEngine::compact_once`/`should_compact` (`pub(crate)`,
no production caller yet), `LsmConfig.compaction_trigger_count`
(default 4, the field the original spec named but the real struct
never had), the engine-agnostic k-way merge + retention algorithm
(`src/compaction/mod.rs`, reusing Increment 6's own persistent
`SsTableRangeCursor` directly), and a generalized, streaming SSTable
writer entry point (`sstable::write_from_sorted_records`) --
`write_from_memtable` is now a thin adapter over the same shared core,
verified **byte-for-byte** behavior-preserving by a new differential
test, not merely logically-equivalent.

**A real correctness refinement found during implementation, not
hidden**: the architecture report's own worked truth table (§11) had
two under-specified rows (`@1`/`@2`) -- correct only under an unstated
"exactly one live snapshot" assumption. `oldest_live_snapshot_seq()`
exposes only the minimum live snapshot seq, never the full set, so the
actual, safe retention algorithm conservatively retains *every*
version from the floor through the newest, not just the floor and the
newest. Caught by the implementation's own unit tests failing against
the originally-planned assertions; corrected in both the tests and via
an added erratum note in the architecture report (original table left
unedited, per this project's append-only convention). The underlying
ADR decision is unchanged -- only two rows' specific numbers were
imprecise.

**Test results**: 26 new tests (13 module-level merge/retention tests,
13 engine-level integration tests) plus 2 new writer differential
tests -- 334/334 total (`cargo test --lib`, debug and release).
Coverage includes: trigger gating and single-table/no-op behavior; a
2,000-op correctness differential against an independent reference
model (never the production algorithm as its own oracle); a 48-case
property test; all 6 `CompactionFaultPoint` crash windows, each a real
injected panic followed by an actual restart (shutdown+drop+reopen),
not an in-process retry; the previously-zero-coverage orphan-recovery
recovery branch (`reconcile_sstables_with_manifest`); concurrent flush;
concurrent readers (4 threads, 2 compaction cycles, zero mismatches);
a real Windows positional-read-after-unlink test (a long-lived range
scan survives its own source table being retired and physically
unlinked mid-scan); and storage-pressure deferral (`compact_once`
never mutates `storage_state`/`storage_pressure_events`, only observes
them). A real race in the shared test fixture helper itself (not
`compact_once`) was found under heavy parallel-test load and fixed by
pacing writes against the flush thread -- documented in the results
doc as a test-infrastructure fix, not a production-code fix.

**Full regression gate clean**: `fmt`/`clippy -D warnings`/`test --lib`
334/334 debug+release/`check`/`wal_tests` 12/12/`crash_consistency`
2/2/`pathological_recovery_matrix` 9/9x2. **Protected-contract audit,
post-implementation**: zero changes to `src/wal/`, `src/error.rs`,
`src/manifest/`, `Cargo.toml`, or `Cargo.lock`; zero `unsafe`
introduced; every existing Read Engine test re-ran unmodified and
green.

**COMPACTION IMPLEMENTATION = INCREMENT 1 COMPLETE.**
**COMPACTION PRODUCTION READY = NO** -- no automatic trigger, no
dedicated performance benchmark, no long-duration soak exercising
compaction, no final certification. **WRITE ENGINE = PRODUCTION
READY** and **READ ENGINE = PRODUCTION READY** remain unchanged,
protected, and re-verified.

## 2026-09-21 (Compaction Increment 2: production trigger + execution integration)

`ADR-COMPACTION-001` Amendment 1 designed and implemented: a real,
automatic background compaction worker, wired to `LsmEngine::open`
behind a new opt-in `LsmConfig.compaction_auto_trigger` flag (default
`false`, deliberately -- see below). Full detail: `PHASE_COMPACTION_
INCREMENT2_RESULTS.md`; full amendment reasoning: `PHASE_COMPACTION_
ADR.md` Amendment 1.

**Delivered**: `spawn_compaction_thread` (background worker, dual wake
source -- flush-triggered notification + periodic fallback tick reusing
the existing `storage_pressure_retry_interval`, no new config field);
`compact_once_impl` (Increment 1's own `compact_once` body extracted
into a free function over cloned `Arc`s, shared byte-identically by
both the manual entry point and the new worker); `CompactionRunGuard`
(one `AtomicBool` RAII guard -- the smallest primitive preventing two
concurrent compactions, no global engine lock); `shutdown()` extended
with a real, explicit contract (new work stops scheduling, an in-
progress cycle always completes, never aborted, no worker leak, no
join deadlock, no partial publish).

**Two real bugs found and fixed empirically, not hypothesized**: (a)
`shutdown()`'s original `try_send(Shutdown)` could silently lose the
stop message against an already-full bounded(1) channel, causing a
real, measured multi-minute slowdown under heavy property-test load
that looked like a hang under a 60s external timeout -- fixed by
switching that one send to blocking `send` (provably non-deadlocking).
(b) A narrow pre-existing race in the shared `put_and_wait_for_
sstable_count` test helper (`immutable_count()==0` alone doesn't prove
a flush job's own tail -- including its unconditional `storage_state`
swap-to-`Healthy` -- has finished) intermittently broke an Increment 1
storage-pressure test; fixed with a new, purely additive `flush_
completions` observability counter.

**A default-value reversal, recorded not hidden**: `compaction_auto_
trigger` was initially implemented defaulting to `true`; reversed to
`false` after two concrete Increment-1 test failures showed a live
background worker would otherwise silently start compacting out from
under a great deal of already-certified, already-passing test surface
across the whole crate the moment this field shipped -- exactly the
"silently weaken an existing guarantee" outcome this project's own
standing principle forbids. `compact_once()`/`should_compact()` remain
directly, manually callable regardless of the flag.

**A genuinely unbounded test-design hazard found and generalized**:
building a fixture by looping `put()` against an *already-running*
worker races the worker's own reaction -- one observed case needed 725
individual writes (not 4) before the test thread's own poll happened
to land in the narrow pre-splice window. Fixed uniformly across every
affected test (6 of 12 new tests) by building offline first
(`compaction_auto_trigger: false`, reusing Increment 1's own
deterministic fixture helper unmodified) then reopening with the
worker enabled -- a one-directional, monotonic wait, never a race
against a thread also trying to reduce the same count.

**Test results**: 12 new tests (`auto_trigger_tests`, nested in
`compaction_tests`) -- 346/346 total (`cargo test --lib`, debug and
release), the new module run twice consecutively (161.33s, 153.33s)
post-fix with zero flakiness. Coverage: deterministic threshold firing
(3/4/5/9 SSTables); direct re-entrancy-primitive stress test (16
threads); storage-pressure defer + auto-resume through the real
worker; failure + automatic retry via a fires-once IO fault hook;
shutdown during a genuinely in-flight cycle (bounded injected delay);
live-snapshot safety across an automatic cycle; deferred-physical-
deletion retry via the worker's own fallback tick; a 400-op bounded
production-like integration run against an independent reference
model, purely automatic; crash recovery reached through the real
automatic path (panic caught by the worker's own `catch_unwind`, a
real restart); bounded resource safety (8 repeated cycles, zero leaked
files, zero worker-thread leak); a first bounded performance/storage-
budget baseline (4/8/16/32/64 input tables -- measured on-disk peak
matched the theoretical `input+output` figure exactly at every size);
bounded write/read latency under concurrent writers+readers+real
automatic compaction (read p99 stayed sub-millisecond throughout).

**Full regression gate clean**: `fmt` (one pass applied, no behavior
change) / `clippy -D warnings` / `test --lib` 346/346 debug+release /
`check --all-targets` / `wal_tests` 12/12 / `crash_consistency`
(`--features test-util`) 2/2 / `pathological_recovery_matrix` 9/9.
**Protected-contract audit**: zero changes to `src/wal/`, `src/
error.rs`, `src/manifest/`, `Cargo.toml`, or `Cargo.lock`; Read Engine
public API surface unchanged; zero new dependency; zero `unsafe`
introduced.

**COMPACTION CORE = PASS** (unchanged, re-verified). **COMPACTION
TRIGGER INTEGRATION = PASS.** **COMPACTION PRODUCTION READY = NO** --
remaining: full performance characterization beyond the bounded
baseline, a real OS-level resource benchmark (RSS/handles/threads,
external to `cargo test`), long-duration write/read/compaction
endurance, a storage-pressure endurance run, and a final certification
matrix. **WRITE ENGINE = PRODUCTION READY** and **READ ENGINE =
PRODUCTION READY** remain unchanged, protected, and re-verified.
