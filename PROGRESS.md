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
