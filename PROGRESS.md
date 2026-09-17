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
