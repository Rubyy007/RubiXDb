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
matrix. **WRITE ENGINE = PRODUCTION READY** and **READ ENGINE = PRODUCTION
READY** remain unchanged, protected, and re-verified.

## 2026-09-22 (Compaction Increment 3: production performance + resource + endurance validation)

**Implemented:** closed every gate `PHASE_COMPACTION_INCREMENT2_
RESULTS.md` §9 left open. One production-code change, purely
additive: `CompactionMetrics`/`LsmEngine::compaction_metrics()`
(`src/lsm/mod.rs`, mirrors `ReadStats`'s own cumulative-counters-plus-
snapshot shape) -- needed because `compact_once`/`should_compact`
remain `pub(crate)` by deliberate, still-honored ADR decision
(`ADR-COMPACTION-001` Decision 13), so an external benchmark/soak
harness has no other way to observe per-cycle `CompactionStats` from
the real automatic worker. Updated only on a successful cycle, never
consulted by any correctness/trigger decision, covered by its own new
unit test. No trigger model, retention rule, Manifest sequence, or
concurrency model change of any kind.

**New harnesses** (`examples/`): `compaction_bench.rs` (performance
sweep 4-256 input SSTables + a 9-shape overlap x value-size sweep,
storage-budget validation, RSS/handle/thread scaling, automatic-
trigger stress, concurrent read/write/compaction correctness,
snapshot and tombstone/version endurance, read/write latency with
compaction idle vs. active, SSTable-count stability, storage-pressure/
failure-retry/shutdown endurance); `compaction_crash_cycle_test.rs` +
`_child.rs` (real external `Child::kill()` crash cycles through the
automatic worker specifically -- a targeted mode that precisely hits
each of the 6 `CompactionFaultPoint`s via a stdout marker technique,
plus a broader random-delay mode); `compaction_soak.rs` (the first
real long-duration integrated production soak with automatic
Compaction active, correctness-checked against an independently
tracked reference model).

**Three real bugs found and fixed in the soak's own correctness
harness** (not in Compaction or the Read Engine -- each traced to its
actual root cause, not assumed): a range-bound wraparound producing
inverted/empty scans near the keyspace boundary; the reference model's
ring buffer breaking its own seq-sorted invariant when two writers
raced the same key (fixed via seq-sorted insertion instead of blind
`push_back`); and a `get()`/`snapshot()`-based correctness comparison
that ran directly into this project's own already-documented
`snapshot_seq()` cross-thread cadence caveat (`read_engine_bench.rs::
section_sanity`) -- confirmed as the same pre-existing, Compaction-
unrelated characteristic (not a new defect) by reproducing it with
**zero** compaction cycles running, then fixed by pinning every
correctness comparison the same proven-race-free way point-checks
already use (a real write's own already-applied seq, obtained via the
model under its own lock, never a cross-thread-sampled watermark).

**Headline results:**
- Storage budget: measured on-disk peak matched the theoretical
  `input+output` figure exactly (0.00% delta) at both 64 and 256
  input tables.
- Concurrent read/write/compaction: 0 mismatches across ~2.37M mixed
  ops and 14 real cycles. Snapshot endurance: 0 mismatches across 39
  cycles. Tombstone/version endurance: 0 mismatches across 12 cycles.
- Compaction measurably *improves* read latency by bounding live
  SSTable count -- point-read p50 dropped ~9x, range p50 dropped
  3-20x, compaction enabled vs. disabled, same workload.
- 38/38 real external-process crash cycles through the automatic
  worker (18 targeted across all 6 `CompactionFaultPoint`s + 20
  random-delay), all recovering cleanly with no orphaned files, no
  regressed watermarks, no read errors.
- RSS grew only 9.7% while cumulative compacted-through data grew
  ~668x and cycle count grew 30x (bounded by live input-table count,
  not cumulative volume, as the ADR intends). Handles/threads returned
  **exactly** to the pre-open process baseline after shutdown across
  100 repeated cycles -- no leak.
- **4-hour production-profile soak** (8 writers, 16 readers,
  `LsmConfig::default()` + automatic Compaction): ~14.9M writes, ~2.6M
  deletes, 3.3 billion point reads, 9.87M range scans, 193 real
  compaction cycles, 17.46M records dropped, RSS stable at 60-70MB
  throughout, live SSTable count never exceeded 3 despite the
  sustained write volume. **0 in-run mismatches, 0 post-recovery
  mismatches** (all 20,000 tracked keys verified correct after a real
  shutdown + reopen).
- A resource-contention false positive was found, traced, and
  resolved rather than silently retried: 2 unrelated `wal::group_
  commit` tests failed once under full parallel-suite load while the
  4-hour soak also ran concurrently on the same machine; both passed
  cleanly in isolation while the soak was still running, confirming
  contention (matching this project's own already-documented precedent
  for exactly this class of finding), not a regression. The
  authoritative final gate was re-run with the soak no longer active.

**Full regression gate clean**: `fmt` / `clippy -D warnings` /
`test --lib` 347/347 debug+release / `check --all-targets` /
`wal_tests` 12/12 / `crash_consistency` (`--features test-util`) 2/2 /
`pathological_recovery_matrix` 9/9. **Protected-contract audit**: zero
changes to `src/wal/`, `src/error.rs`, `src/manifest/`, `src/
compaction/mod.rs`, `Cargo.toml`, or `Cargo.lock`; zero new
dependency; zero `unsafe` introduced.

**COMPACTION INCREMENT 3 = PASS.** **COMPACTION PRODUCTION READY =
NO** -- final certification (mirroring `PHASE_READ_ENGINE_
CERTIFICATION.md`'s own structure) is explicitly out of scope for this
increment, a new, separately-scoped increment. **WRITE ENGINE =
PRODUCTION READY** and **READ ENGINE = PRODUCTION READY** remain
unchanged, protected, and re-verified by this increment's own full
regression gate.

## 2026-09-22

**Implemented:** Productization phase -- a Service API layer and a
frontend console built *on top of* the certified engine, per
`PHASE_API_ARCHITECTURE.md`/`PHASE_API_IMPLEMENTATION.md` and
`PHASE_FRONTEND_ARCHITECTURE.md`/`PHASE_FRONTEND_IMPLEMENTATION.md`.
This is new surface area, not a change to the certified engine: the
architecture is Client -> HTTP API -> Service layer -> the unmodified
`LsmEngine`. Router, Replication, Partitioning, and leveled compaction
were explicitly not started, per the phase's own stop condition.

- **`rubixdb-api`** (new `api` workspace member, `axum` 0.7 + `tokio`):
  bearer API-key auth with a `reader`/`admin` role hierarchy, per-
  principal token-bucket rate limiting, a typed `EngineError` ->
  HTTP-status/code mapping that never leaks a raw `io::Error` or
  filesystem path into a response body, a snapshot lifecycle service
  wrapping the engine's own RAII `Snapshot`, bounded graceful shutdown
  (`server::serve()`, injectable shutdown trigger, tested directly
  rather than relying on real OS signal delivery), per-route p50/p95/
  p99 latency metrics, and a CORS layer (`RUBIXDB_CORS_ALLOWED_
  ORIGINS`, empty/same-origin-only by default, never wildcards origin).
  Full route surface: health/readiness/whoami, status/metadata, KV
  put/get/delete/exists (with `?as_of_seq=` historical reads), range
  scans, snapshot create/list/get/release, compaction status/metrics
  (read-only -- no force-compaction control exists, because no such
  engine API exists), and combined service+engine metrics.
- **`frontend`** (new directory, React 18 + TypeScript + Vite, no
  component-library dependency): a database console with Dashboard,
  Data Explorer (point lookup + range query, consolidating "Query/
  workspace" and "Result viewer" since no SQL layer exists to give
  those separate meaning), Snapshots, Compaction (status/metrics only,
  no manual-trigger control), Health/Storage (its per-route metrics
  table doubles as the "Logs/errors" screen -- no log-retrieval
  endpoint exists on the backend to back a real log viewer), and
  Settings. Design tokens (light/dark via `prefers-color-scheme` +
  override), a small hand-built component set, `@tanstack/react-query`
  for server state, one `SessionContext` for the only genuinely-global
  client state (session-storage by default, local-storage only on
  explicit opt-in).
- **One backend gap closed for the frontend's sake, documented rather
  than silently added:** `GET /v1/whoami`, so the console can learn
  its own authenticated role -- zero change to the auth model itself.
- **Five real bugs found by real (unmocked) testing, fixed at the
  source:** a stale-read-cache bug (`PUT`/`DELETE` did not invalidate
  cached `["kv", ...]` react-query reads, fixed via explicit
  invalidation in `api/queries.ts`), two `axe-core`-caught color-
  contrast failures (`--color-healthy`, `--color-text-faint`,
  darkened in `tokens.css`), a heading-hierarchy skip (`Card` titles
  now render as `<h2>`, not `<h3>`), and a missing `<main>` landmark on
  the pre-session Connect screen.

**Tests passing:** Engine (unchanged, re-verified after every change
batch): 347/347 lib tests debug+release, `wal_tests` 12/12,
`crash_consistency` 2/2, `pathological_recovery_matrix` 9/9, fmt/
clippy/`check --workspace --all-targets --all-features` all clean.
Backend (`rubixdb-api`): 29/29 unit tests, 15/15 real integration tests
(`tower::ServiceExt::oneshot` against the real router + real engine,
including a real-process-restart persistence test and a CORS test).
Frontend: 17/17 vitest unit/component tests, 10/10 Playwright e2e
tests against the real built frontend + real `rubixdb-api.exe` binary
on a separate origin (full workflow, role-gating, invalid-key
rejection, `axe-core` accessibility audit of all 7 screens, responsive
viewport checks at desktop/tablet/small). Production frontend build
succeeds (`dist/index.html` 0.45 kB, CSS 10.27 kB/2.62 kB gzip, JS
239.59 kB/74.56 kB gzip).

**Protected-engine audit:** `git diff --stat -- src/` empty across the
entire phase -- zero changes to WAL, Write Engine, Read Engine,
Compaction, SSTable/Manifest format, or snapshot semantics.

**Explicitly not done / not declared:** Router, Replication,
Partitioning, and leveled compaction were not started. Overall RubiXDB
production readiness is **not** declared by this phase -- **WRITE
ENGINE**, **READ ENGINE**, and **COMPACTION** remain independently
**PRODUCTION READY** per their own certifications; the new API/
frontend layer's readiness rests on its own test evidence above, not
on the engine's.

## 2026-09-22 (Relational database: Phase 0/1 architecture audit)

**Implemented:** a read-only audit of the certified engine, API, and
frontend, followed by the relational-layer architecture itself --
`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md`, `PHASE_RELATIONAL_
DATABASE_ADR.md` (33 decisions, each with Decision/Reason/Alternatives/
Correctness/Security/Performance/Memory/Persistence/Recovery/Testing),
and `PHASE_RELATIONAL_STORAGE_GAP_ANALYSIS.md`. **Zero implementation
code** -- `git status`/`git diff --stat` confirmed only the three new
`.md` files existed at the end of this phase.

**Process note:** the first-pass storage-engine audit fork exceeded its
read-only instructions and wrote the three documents itself, in
parallel with and blind to a separate API/frontend audit fork's
findings. A dedicated verification pass (real source cross-checked
against both audits' claims) found the result held up factually, with
one real gap (Observability had no full ADR decision record) -- fixed
by adding D33. Recorded here as a one-off process deviation, not a
pattern.

**Governing finding:** the certified engine has no atomic multi-key
write primitive -- `put`/`delete` are single-key; the batch coordinator
only amortizes fsyncs across independent callers, granting no cross-key
atomicity. Every relational guarantee (transactions, index/table
consistency, DDL) depends on this not existing yet being resolved
first. Everything else in the ADR is implementable entirely on the
certified engine's existing, unchanged public surface.

**RELATIONAL IMPLEMENTATION = NOT STARTED. RELATIONAL DATABASE
PRODUCTION READY = NO.** Write/Read/Compaction remain independently
PRODUCTION READY, unaffected.

## 2026-09-22 (Relational database: ADR Amendment 001 + Increment 2 -- `write_batch`)

**Implemented:** an external review reordered the plan -- the atomic
storage primitive (D9) must be built and certified before the catalog/
tables/indexes, not alongside them. Before touching `src/`,
`RELATIONAL ADR AMENDMENT 001` was appended to `PHASE_RELATIONAL_
DATABASE_ADR.md` (append-only -- D1-D33 untouched in substance),
resolving every question D9's original sketch had left open by reading
the actual certified WAL/MemTable/BatchCoordinator source directly
rather than re-deriving from D9's own summary of it: sequence semantics
(AA.1, one shared seq per batch -- the forced consequence of the
existing one-seq-per-frame WAL layout and `(key,seq)`-keyed MemTable),
WAL frame format (AA.2, additive `OP_GROUP` tag, zero changes needed to
`wal::recovery`'s frame classification), an atomic-visibility proof
traced against the real `RwLock` (AA.3), same-key resolution (AA.4),
required performance properties (AA.5), resource limits (AA.6), failure
semantics (AA.7), concurrency integration (AA.8), and a security review
(AA.9) -- plus catalog-security (AA.10), a documentation-consistency
fix (AA.11: `system.grants` was missing from the Architecture doc's
table list, now matches the ADR), upgrade-safety (AA.12), the SQL
execution model (AA.13), and precision corrections to two pieces of
imprecise wording (AA.15: PK/index lookup complexity claims now state
their live-SSTable-count dependency inline, not only in a separate
table column; AA.16: "B-tree-shaped" corrected to "ordered LSM-backed,"
since no B-tree implementation exists or is planned).

Then `PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md` (narrowly scoped,
cites the amendment rather than re-deriving it) and the actual
`write_batch` implementation: `LsmEngine::write_batch(&self, ops:
&[WriteOp]) -> Result<u64>`, one new WAL op tag (`OP_GROUP = 5`,
additive, existing frame envelope unchanged), `apply_batch_after_
durable` (the same `RwLock` write guard `apply_after_durable` already
takes, held for N inserts instead of one), `LsmConfig::max_batch_ops`
(default 10,000), and one new `EngineError::InvalidArgument` variant
(plus its one mechanical cross-crate consequence in `api/src/error.
rs`'s exhaustive match).

**Proven, not just argued:** a concurrent-reader test
(`concurrent_reader_never_observes_a_partial_batch`) races a real
`write_batch` call held mid-critical-section against a single-lock-
acquisition `range_scan` covering every touched key, across 30
interleavings -- zero partial observations. (An earlier version of
this test used four independent `get_as_of` calls instead and produced
apparent failures; investigated and found to be a test-design flaw, not
an engine bug -- four separate lock acquisitions can legitimately
straddle a writer's critical section, which is a real property of
issuing four independent reads, not partial-batch visibility. Fixed by
redesigning the test around one atomic `range_scan`, not by weakening
the assertion.) A differential/property test compares `write_batch`
against an independent, serialized `BTreeMap` reference model across
random batch contents including duplicate/same-key sequences (64
proptest cases). Recovery tests confirm live-apply and replay reach
identical state via one shared helper function, not independently-
maintained-but-hopefully-equivalent code paths.

**Measured, not claimed** (`cargo bench --bench write_batch_bench`,
release profile): N=1 parity confirmed (`put` 6.24ms vs. `write_batch`
6.39ms; `delete` 12.92ms vs. 12.99ms -- overlapping confidence
intervals). N>1 throughput: `write_batch` stays ~4.2-5.5ms regardless
of batch size (one fsync) while N sequential `put` calls from one
caller scale linearly -- up to 57x faster at N=64.

**Tests passing:** `cargo test --lib` 373/373 (debug and release),
`wal_tests` 12/12, `pathological_recovery_matrix` 9/9, `crash_
consistency --features test-util` 2/2, fmt/clippy (`-D warnings`)/
`check --workspace --all-targets --all-features` all clean. 14 new
`write_batch`-specific engine tests, 13 new WAL `Group`-frame tests, all
passing.

**Flagged, investigated, confirmed pre-existing, not touched:**
`group_commit`'s `m1_2_hundred_writers_throughput`/`m1_3_thousand_
writers_throughput` fail on this machine in both debug and `--release`
(7,330 vs. a 15,000 target; 45,395-52,460 vs. an 80,000 target).
Verified via `git stash` that the identical failure, with near-
identical numbers, occurs on the clean, unmodified baseline -- a
pre-existing, machine-throughput-dependent characteristic, not a
regression from this work. Not weakened, not silently ignored.

**Protected-engine audit:** `git diff --stat -- src/manifest/ src/
compaction/ src/sstable/` empty -- zero changes to Manifest,
Compaction, or SSTable.

**Explicitly not done / not declared:** no catalog, schema, table,
index, SQL parser, transaction executor, or CLI exists. **RELATIONAL
DATABASE PRODUCTION READY = NO.** Write/Read/Compaction remain
independently PRODUCTION READY, unaffected -- confirmed by the full
regression gate above, not merely asserted. Full account: `PHASE_
RELATIONAL_TRANSACTION_STORAGE_RESULTS.md`.

## 2026-09-22 (Relational database: Increment 3 -- persistent catalog)

**Implemented:** before writing code, `RELATIONAL ADR AMENDMENT 002`
(appended to `PHASE_RELATIONAL_DATABASE_ADR.md`, D1-D33 and AMENDMENT
001 untouched) resolved the catalog's own remaining open points that
neither the ADR nor the Architecture document had pinned down to a
concrete, implementable byte layout: exact per-system-table column
schemas and primary keys (CA.2), durable/restart-safe/collision-free ID
allocation with no D10 transaction layer yet to lean on for conflict
detection (CA.1), `system_table_id` constant assignment, catalog
bootstrap (CA.3), and this increment's own explicitly-scoped-down DROP
semantics -- direct atomic catalog-row removal, not D13's full
`DROPPING`-marker-plus-background-sweep design, since no table-row
storage exists yet for a sweep to have anything to act on (CA.4).

Then the implementation: `src/catalog/` (`encoding.rs`, `schema.rs`,
`service.rs`, `error.rs`) -- the seven `system.*` tables (D1) as
ordinary rows in the certified `LsmEngine`'s own keyspace under the
reserved `0x00` namespace (D2), every mutation going through exactly
one certified `write_batch` call (D9/D13), every read an ordinary
`get`/`range_scan`, no separate catalog cache, no new WAL, no
independent catalog file. `CatalogService` provides `bootstrap`
(idempotent), `create_schema`/`create_table`/`create_index`/
`create_constraint`/`grant`/`revoke`, matching reads, and `drop_table`/
`drop_index`/`drop_schema`.

**ID allocation** (the review directive's own explicit focus): every
ID is read from and incremented as an ordinary durable catalog row --
never a process-local counter as the source of truth, no hash
derivation, no external sequence service. A new, narrowly-scoped
`Mutex` internal to `CatalogService` (not a change to the engine's own
locking model) serializes this process's own catalog-mutating calls --
a real fix for a real race `write_batch`'s atomicity alone cannot
close (two concurrent `CREATE TABLE` calls reading the same "next
table_id" before either commits would otherwise both succeed, both
durable, both claiming the same physical namespace region). Verified
directly: 16 concurrent `CREATE TABLE` calls never produce a duplicate
`table_id`; `table_id` allocation continues from its durable value
after a real restart, never resets.

**Proven, not just argued:** `create_table`'s multi-row mutation (table
row + N column rows + PK index row + two ID counters) is exactly one
`write_batch` call, not several independent `put`/`delete` calls --
verified by asserting the engine's own sequence counter advances by
exactly one per `create_table`, regardless of column count (`write_
batch`'s own certified AA.1 property, exercised through the catalog
layer). A concurrent-scan test races 25 rounds of `create_table` against
concurrent `get_table_by_name`/`get_columns`/`list_indexes` reads,
asserting a table is never visible with some but not all of its
children present -- zero inconsistent observations.

**Tests:** 43 new (`cargo test --lib catalog::`) -- encode/decode
round-trips for every system table and `NULL`-bitmap boundary,
namespace isolation from pre-existing flat-KV keys (D2/D32), restart/
recovery (catalog rows and ID counters both survive a real engine
close/reopen), `DROP TABLE` cascade with cross-table isolation, grants
uniqueness (duplicate grant rejected, revoke is idempotent), and
invalid-input rejection (empty name, no columns, no primary key,
nullable PK column, duplicate column names, out-of-range ordinals).

**Full regression gate, before and after this increment:** `cargo fmt
--all -- --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings`, `cargo test --workspace` and `--release
--workspace` -- 416 `rubixdb` lib tests + 30 `rubixdb-api` tests, debug
and release, all passing. `wal_tests` 12/12, `pathological_recovery_
matrix` 9/9, `crash_consistency --features test-util` 2/2 (release).
`src/manifest/`, `src/compaction/`, `src/sstable/`, `src/wal/`, `src/
error.rs`, `api/` completely untouched (`git diff --stat` empty for
each) -- this increment's only source changes are the new `src/
catalog/` module and one added `pub mod catalog;` line in `src/lib.rs`.
No new external dependency (`Cargo.toml`/`Cargo.lock` unchanged).

**Flagged, confirmed pre-existing, not caused by this increment:** the
same `group_commit` throughput test pair (`m1_2_hundred_writers_
throughput`/`m1_3_thousand_writers_throughput`) already flagged as a
pre-existing, machine-throughput-dependent baseline characteristic in
Increment 2 (verified there via `git stash` against the clean baseline)
recurs identically here. `tests/` has zero diff from this increment
(`git diff --stat -- tests/` empty), confirming it cannot be this
increment's own doing.

**Security audit:** no `unsafe`, no new logging of key/value/catalog-
row contents, no unbounded allocation from untrusted input (the one
`Vec::with_capacity` sized by a decoded length, in `decode_u16_list`,
is capped at 65,536 elements regardless of what the decoded count
claims), no new panic paths in non-test code, no new external
dependency.

**Explicitly not done / not declared:** no SQL parser/binder/executor,
no `CREATE TABLE` SQL syntax, no `INSERT`/`UPDATE`/`DELETE` execution,
no user-table row storage, no authorization *enforcement* (`system.
grants` rows are stored; nothing yet checks them -- D15's binder,
a later increment). **RELATIONAL DATABASE PRODUCTION READY = NO.**
Write/Read/Compaction remain independently PRODUCTION READY,
unaffected.

## 2026-09-22 (Relational database: Increment 4 -- row-storage foundation)

**Implemented:** before writing code, `RELATIONAL ADR AMENDMENT 003`
(appended to `PHASE_RELATIONAL_DATABASE_ADR.md`, D1-D33/AMENDMENTs
001-002 untouched in substance) resolved this increment's own remaining
open points: exact order-preserving key transforms for every D4 type at
the byte level (D4's Reason text named the *technique* for each type --
sign-flip, monotonic bit-transform -- but not the exact bytes), an
escape-then-terminate encoding for `TEXT`/`BLOB` inside a composite key
(proven correct by direct case analysis, not merely cited as standard),
and how `DECIMAL(p,s)`'s precision/scale gets persisted given `system.
columns.data_type:u8` (CA.2) alone had no room for a parameterized
type's own parameters.

Then the implementation: `src/relational/` (`value.rs`, `key.rs`,
`table_store.rs`, `error.rs`) -- `RelationalValue`/`RelationalType`
(D4's full closed type set), order-preserving key encoding for every
type, the table-row physical key layout implemented exactly as
Architecture document §5 already specified (`0x01 || table_id:u32 BE ||
0x00000000:u32 BE || encoded_pk`), and `TableStore` --
`put_row`/`put_rows`/`get_row`/`delete_row`/`scan_table`, every
mutation through exactly one certified `write_batch` call (even at
N=1), every read resolving the table's shape from the unmodified
`CatalogService` (no second metadata structure, no cache).

**One additive catalog extension** (`system.columns.type_params`, a new
trailing `BLOB` field): `DECIMAL`'s `(precision, scale)` has nowhere
else to live. D31-licensed (a new trailing field on an existing row
schema is exactly the additive shape D31 already declared safe), not a
redesign -- every pre-existing `system.columns` field is untouched, and
the full existing 44-test catalog suite was re-run unmodified and still
passes. `catalog::encoding`'s `RowValue` envelope (header/null-bitmap
logic) was also refactored into `encode_row_envelope`/`decode_row_
envelope`, generic over the value type, so catalog rows and relational
rows now share the *identical* codec implementation rather than two
independently-maintained copies of the same on-disk format -- verified
by re-running the catalog suite unmodified after the refactor (same
44/44 passing, confirming the on-disk format itself did not change).

**Proven, not just argued:** every ordering transform (integer,
`BIGINT`, `DECIMAL`, `DATE`, `TIMESTAMP`, `REAL`, `DOUBLE`, `TEXT`,
`BLOB`) is checked by a proptest property comparing `byte_lexicographic_
compare(encode(a), encode(b))` against an independently-computed
`logical_compare(a, b)` -- never from inspection alone, per the review
directive's own explicit instruction. The specific `-0.0`/`+0.0`
edge case (the same value under IEEE-754 equality, so their encodings
must be byte-identical, which the naive sign-bit transform gets wrong
without an explicit canonicalization step) is caught by a dedicated
test, not discovered later. A table scan's physical range boundaries
are verified directly (neighboring/min/max `table_id` byte comparisons),
not only by post-filtering. `put_row`/`delete_row` issuing exactly one
`write_batch` call is verified by asserting the engine's own sequence
counter advances by exactly one per call, regardless of column count.

**Tests:** 79 new (`cargo test --lib relational::` plus one new catalog
test) -- row/key round-trips for every type, ordering property tests,
composite-key correctness (including `TEXT` not in the last position,
the case that actually requires the terminator scheme rather than raw
bytes), table-scan namespace isolation, cross-namespace isolation from
catalog rows and flat-KV keys, invalid-input rejection (wrong value
count, `NULL` primary key, `NOT NULL` violation, type mismatch,
`DECIMAL` precision overflow), oversized-row rejection (verified no WAL
record written for the rejected attempt), restart persistence (insert,
delete, and scan all re-verified after a real engine close/reopen),
concurrent `put_row` (16 threads), concurrent scan during writes (never
a torn row), a delete/read race (never an `Err`), and a differential
test against an independent, serialized `BTreeMap` reference model (48
proptest cases).

**Measured, not claimed** (`cargo bench --bench table_store_bench`,
release profile): the write path (`put` vs. `put_row`) is `fsync`-
dominated on this machine and shows no measurable difference either
way. The read path (not `fsync`-bound) shows a real, measured ~20x cost
(329ns raw `get` vs. 6.74µs `get_row`) -- attributed honestly to `get_
row`'s per-call, uncached catalog resolution (RA.3's deliberate v1
design, not yet optimized for repeated access), not hidden or
downplayed. A 10,000-row `scan_table` case was attempted, found to take
upwards of 15 minutes (dominated by 10,000 sequential individual-fsync
`put_row` calls in the benchmark's own setup, not the scan itself), and
abandoned rather than reported with no real number -- 100/1,000-row
results are real, complete measurements.

**Full regression gate, before and after this increment:** `cargo fmt
--all -- --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings`, `cargo test --workspace` and `--release
--workspace` -- 468 `rubixdb` lib tests + 30 `rubixdb-api` tests, debug
and release, all passing. `wal_tests` 12/12, `pathological_recovery_
matrix` 9/9, `crash_consistency --features test-util` 2/2 (release).
`src/manifest/`, `src/compaction/`, `src/sstable/`, `src/wal/`, `api/`
completely untouched (`git diff --stat` empty for each).

**Security audit:** no `unsafe`, no new logging of key/value/row
contents, no unbounded allocation from untrusted input (every `Vec::
with_capacity` in the new code is sized from already-materialized
caller data, never a decoded on-disk integer), no new panics in
non-test code beyond provably-infallible `try_into().expect(...)`
conversions on already-length-validated slices (the same pattern the
certified WAL decoder already uses), no new external dependency.

**Explicitly not done / not declared:** no SQL parser, binder,
executor, `CREATE TABLE`/`INSERT`/`UPDATE`/`DELETE`/`SELECT` SQL, query
planning, joins, aggregation, or authorization enforcement. Index
maintenance (D7/D11) is not wired into `put_row`/`delete_row` yet --
deliberately shaped to need no call-site change when it is added.
**RELATIONAL DATABASE PRODUCTION READY = NO.** Write/Read/Compaction
remain independently PRODUCTION READY, unaffected. Full account:
`PHASE_RELATIONAL_ROW_STORAGE_RESULTS.md`.

---

## 2026-09-23

**Implemented:** Increment 5 -- production secondary indexes with
online `CREATE INDEX`. `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` is the
full decision record. `put_row`/`put_rows`/`delete_row` now maintain
every `Building`/`Ready` secondary index atomically (one `write_batch`
per row op, D11) -- the exact wiring `PHASE_RELATIONAL_ROW_STORAGE_
RESULTS.md` had deliberately left for this increment. A new per-table
"index epoch lock" (`TableStore::epoch_lock`, one `RwLock<()>`, reusing
the same mutual-exclusion proof technique already certified for
`write_batch`'s own atomicity) closes two concurrency races: a writer
using a stale index list across the `Building` catalog transition, and
a "phantom entry" race where a stale backfilled write could resurrect
an entry for a row deleted mid-build. Both are proven in the ADR and
directly tested with barrier-synchronized (never sleep-based)
concurrency.

**Found and fixed along the way:** a latent boundary bug in
`relational::key::table_row_range` -- it bounded a table scan by the
entire `table_id` key prefix, which silently included secondary-index
entries (sharing that same prefix, `index_id > 0`) once they existed.
Caught by this increment's own tests, not by inspection first.

`IndexState` extended from `{Active, Building}` to `{Ready, Building,
Failed, Dropping}` (`Active` renamed `Ready`, same on-disk tag).
`DROP INDEX` is a bounded, resumable physical sweep (D13's `DROPPING`-
table precedent, mirrored). Crash recovery restarts an interrupted
build or sweep from scratch (never resumes from an unproven cursor,
never silently promotes to `Ready`). `IndexBuilder::index_lookup`/
`index_range_scan` are real index-then-fetch access paths -- measured
~467x faster than an equivalent full table scan for a 1-in-5,000
selective predicate. `UNIQUE` index physical structure exists;
enforcement is correctly deferred to the not-yet-built transaction
layer (D10), stated honestly, not faked.

31 new tests (concurrency, crash recovery, restart persistence, a real
automatic-compaction interaction test -- 49 real compaction cycles
observed racing an in-progress backfill in one run -- resource limits,
corruption handling). Full regression: 497 `rubixdb` + 30 `rubixdb-api`
tests, debug and release, all passing; `wal_tests`/`pathological_
recovery_matrix`/`crash_consistency` unchanged. `src/manifest/`,
`src/compaction/`, `src/sstable/`, `src/wal/`, `api/` completely
untouched -- no new storage-engine primitive was required; the whole
protocol is built from the already-certified `snapshot`/`range_scan(...,
as_of_seq)`/`write_batch`.

**RELATIONAL DATABASE PRODUCTION READY = NO.** Write/Read/Compaction
remain independently PRODUCTION READY, unaffected. Full account:
`PHASE_RELATIONAL_INDEX_INCREMENT5_RESULTS.md`.

---

## 2026-09-23 (later)

**Implemented:** Increment 6 -- production SQL parser + internal AST +
binder + authorization resolution, in a new `rubixdb-sql` workspace
crate (`sql/`), depending on the core `rubixdb` crate and `sqlparser =
"=0.63.0"` (never the reverse -- the core engine crate's own "zero new
dependency" property is unaffected; `git diff --stat -- src/` for this
increment is empty). `PHASE_RELATIONAL_SQL_GRAMMAR.md` is the
structural/reference record, `PHASE_RELATIONAL_SQL_INCREMENT6_RESULTS.md`
the certification matrix. No SQL execution exists -- every `Statement`
variant's own doc comment states PARSED/BOUND/NOT EXECUTABLE YET.

The binder resolves catalog identifiers and D25 authorization in the
*same* pass (`bind::scope::resolve_table`) -- "does not exist" and
"exists but forbidden" are structurally the same `UnknownObject` error,
verified directly, never distinguishable to a caller. `JOIN` binding
(`INNER`/`LEFT` only, D18), wildcard expansion using real catalog column
order, and `BETWEEN`/`IN`/comparison type unification (a rigid
column/function type always wins over a flexible literal's own
context-free default, regardless of expression side) are all real,
tested logic, not stubs.

**A real vulnerability found and fixed**: `sqlparser`'s own recursion
guard protects its *parsing* call stack, but a long flat chain of binary
operators (`1 + 1 + 1 + ...`) is Pratt-parsed iteratively, never
tripping that guard, while still building a correspondingly deep
`Box<Expr>` tree -- Rust's ordinary recursive `Drop` for that tree then
overflows the stack. Reproduced deterministically (a 20,000-term chain
crashed the test binary with `STATUS_STACK_OVERFLOW`, not a returned
error) before being closed with a pre-parse operator-density check
(`parse::reject_pathological_operator_chains`) -- the only available
mitigation without modifying `sqlparser` itself.

98 new tests (parser correctness, resource-limit boundaries,
`proptest`-driven fuzzing including the stack-overflow regression,
binder integration against a real catalog, SQL-injection and
authorization-bypass security tests, and an independent reference-model
differential test for column resolution -- zero mismatches across a
fixed matrix plus 64 generated cases). Full regression: 497 `rubixdb` +
30 `rubixdb-api` + 98 `rubixdb-sql` tests, debug and release, all
passing; `wal_tests`/`pathological_recovery_matrix`/`crash_consistency`
unchanged. `src/manifest/`, `src/compaction/`, `src/sstable/`,
`src/wal/`, `api/`, and the rest of `src/` are completely untouched.

Also filled a genuine gap: `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md`
§1 promised a dedicated "Identifier Rules" section in the ADR that was
never actually written -- supplied in `PHASE_RELATIONAL_SQL_GRAMMAR.md`
§9, using exactly the case-folding behavior the Architecture doc's own
prose already committed to.

**RELATIONAL DATABASE PRODUCTION READY = NO.** Write/Read/Compaction/
catalog/row-storage/secondary-indexes remain independently certified,
unaffected. Full account: `PHASE_RELATIONAL_SQL_INCREMENT6_RESULTS.md`.

---

## 2026-09-23 (later still)

**Implemented:** Increment 7 -- a production-grade Snapshot Isolation
transaction engine (`src/relational/txn.rs`, new): `TransactionManager`/
`Transaction` executing D10 exactly as already approved. `BEGIN` pins
one `LsmEngine::Snapshot`; reads resolve against a local write-set
overlay first, the pinned snapshot second (read-your-own-writes);
writes stay buffered until `COMMIT`, which re-validates every touched
physical key's freshness (value comparison, `get_as_of` at snapshot vs.
current -- the same check catches `PRIMARY KEY` conflicts with no
special-casing), enforces `UNIQUE` for real for the first time (a
physical existence scan reusing Increment 5's own index structure, with
intra-transaction-duplicate and self-vacated-entry races both closed,
and standard-SQL "`NULL` never conflicts with `NULL`" semantics), then
applies the whole write-set -- table row and every affected index entry
together -- through one `LsmEngine::write_batch` call. `ROLLBACK` is
O(1): nothing was ever durable to undo. `commit(self)`/`rollback(self)`
consume `self` by value, making "commit twice" a Rust compile error,
not a runtime check.

Commit's own critical section serializes on Increment 5's per-table
`epoch_lock` (write side), acquired for every touched table in sorted
order (deadlock-free across concurrent multi-table transactions) --
reused, not duplicated. This is a **table-level**, not key-level, lock:
measured directly (`txn_concurrent_commits_disjoint_keys`), commit
throughput on one table stays flat (~260-290 commits/sec) from 1 to 32
concurrent committing threads even on fully disjoint keys. Reported
honestly as a deliberate tradeoff, not hidden.

**Write skew is possible under Snapshot Isolation** -- documented and
directly demonstrated (`write_skew_is_possible_under_snapshot_
isolation`, the two-on-call-doctors scenario), never claimed fixed;
this is Snapshot Isolation, not Serializable isolation.

42 new tests: lifecycle, read-your-own-writes, snapshot consistency,
conflict detection (same-key, `PRIMARY KEY` both orderings, `UNIQUE`
both orderings, intra-transaction duplicates, self-vacated re-inserts,
`NULL` semantics, multiple independent `UNIQUE` indexes), atomic
table+index commit (verified via engine-seq-delta, RA.5's own
technique), autocommit, write skew, five deterministic barrier-
synchronized concurrency tests (never sleep-based), three resource-
limit boundaries, snapshot/registry lifetime, real automatic-compaction
interaction, two real-process-restart crash-recovery tests, metrics
accounting, an authorization-boundary test, and two differential tests
against an independent from-scratch Snapshot Isolation reference model
-- a fixed scenario and a **randomized, interleaved `proptest`** across
multiple simultaneously-open transaction slots. Full regression: 537
`rubixdb` + 30 `rubixdb-api` + 98 `rubixdb-sql` tests, debug and
release, all passing; `wal_tests`/`pathological_recovery_matrix`/
`crash_consistency` unchanged; `src/manifest/`, `src/compaction/`,
`src/sstable/`, `src/wal/`, `api/` completely untouched.

`PHASE_RELATIONAL_TRANSACTION_ARCHITECTURE.md` is the full decision
record, `PHASE_RELATIONAL_TRANSACTION_INCREMENT7_RESULTS.md` the
certification matrix and measured `cargo bench --bench transaction_
bench` numbers.

**RELATIONAL DATABASE PRODUCTION READY = NO.** No SQL parser execution,
planner, optimizer, executor, CLI, HTTP API, or frontend exists --
this increment's own explicit stop condition. Write/Read/Compaction
remain independently PRODUCTION READY, unaffected. Full account:
`PHASE_RELATIONAL_TRANSACTION_INCREMENT7_RESULTS.md`.

---

## 2026-09-23 (final)

**Implemented:** Increment 8 -- a production-grade, rule-based query
planner and optimizer (`sql/src/plan/`, new module in the existing
`rubixdb-sql` crate): `BoundStatement -> LogicalPlan -> (one fixed-order
optimization pass) -> PhysicalPlan`, executing D16/D17/D18 exactly as
already approved. `PRIMARY KEY` lookup detection is composite-safe (`a
= ?` for `PRIMARY KEY(a, b)` correctly never becomes a point lookup
unless `b` is also constrained -- directly, adversarially tested both
ways). Secondary-index selection respects declared leading-column
order, only ever considers `Ready` indexes, and -- a real, inspected
storage fact, not a guess -- never selects a `Primary`-kind catalog
index as an `IndexScan`, since `TableStore`'s own `maintained_indexes`
filter means no physical entries are ever written for one; `PRIMARY
KEY` access always routes through the dedicated `PkLookup` path
instead. Every `PhysicalAccess` variant maps to a real, already-
implemented storage primitive (`TableStore::get_row`, `IndexBuilder::
index_lookup`/`index_range_scan`, `TableStore::scan_table`) -- nothing
invented.

Predicate pushdown never rewrites `BoundExpr` logic, only relocates
where an unmodified subtree is evaluated -- three-valued logic is
preserved automatically, and a `LEFT JOIN`'s nullable-side predicate is
never pushed into its own scan (directly, adversarially tested: `WHERE
orders.amount = 100` on a `LEFT JOIN`'s right side stays a `Filter`
above the `Join`, never migrates into `orders`' own scan). `ORDER
BY`/`Sort` elimination is conservative and exact-match only -- and,
during this increment's own development, a wrong assumption ("ascending
index scans satisfy a bare `ORDER BY`") was caught by a failing test:
D5's own SQL-standard default for unspecified `NULLS` is `NULLS LAST`
ascending, the *opposite* of the index's own physical `NULLS FIRST`
encoding (no reverse-scan primitive exists anywhere in `LsmEngine`), so
only an explicit `ORDER BY col NULLS FIRST` actually eliminates `Sort`
-- both directions now directly tested. `UPDATE`/`DELETE` reuse
`SELECT`'s own access-planning algorithm verbatim, one implementation
never duplicated. `INNER`/`LEFT JOIN` via Nested Loop with mechanical
Index Nested Loop substitution (detected via a correlated `BoundExpr::
Column` reference to the outer side -- no new expression type
invented); the full `ON` condition always still evaluated in full by a
future executor regardless of what the inner access already consumed
as a candidate-narrowing key.

**A second real, pre-existing bug found and fixed** (the same "found by
writing adversarial tests, not by inspection" pattern as Increments 5
and 6): while writing this increment's own adversarial planner test,
binding a mere 20-term `WHERE ... OR ...` chain was found to take ~4
seconds and growing exponentially (~1.92x per added term) -- traced to
`sql/src/bind/expr.rs::bind_shared` (Increment 6), whose second bind
pass unconditionally re-bound every operand, including already-rigidly-
typed subtrees, doubling the work at every nesting level of a left-deep
chain (`O(2^depth)`, not `O(depth)`) -- a real, exploitable CPU-
exhaustion vector reachable with an ordinary, resource-limit-compliant
`WHERE` clause (well within `SqlLimits::max_expression_depth`, so the
existing depth guard alone did not stop it). Fixed: only a flexible
literal operand needs re-binding; a rigid expression's type is merely
re-checked, not re-walked. Verified: a 100-term chain now binds in
<1ms (previously would have taken ~10^17 seconds at the old growth
rate); D21's "no implicit coercion" correctness re-verified unchanged.

42 new tests across three files (`sql/src/plan_tests.rs`,
`plan_reference_model.rs`, plus 3 regression tests in
`bind_tests.rs`): logical/physical separation, PK/index/range/residual
correctness, LEFT JOIN safety, NULL semantics, projection pruning
(honestly reported as metadata-only -- no partial-column-decode
storage primitive exists yet to attach a real optimization to), limit/
sort analysis in every direction, DISTINCT, join algorithm selection,
UPDATE/DELETE reuse, DDL/transaction-control pass-through, EXPLAIN
determinism, resource limits, metrics, adversarial deep predicates,
race-free concurrency, and a randomized differential property test
against an independent reference model. Full regression: 537 `rubixdb`
+ 30 `rubixdb-api` + 143 `rubixdb-sql` tests, debug and release, all
passing; `wal_tests`/`pathological_recovery_matrix`/`crash_consistency`
unchanged; `src/manifest/`, `src/compaction/`, `src/sstable/`,
`src/wal/`, `api/`, and `src/relational/txn.rs` completely untouched.

`PHASE_RELATIONAL_QUERY_PLANNER_ARCHITECTURE.md` is the full decision
record, `PHASE_RELATIONAL_QUERY_PLANNER_INCREMENT8_RESULTS.md` the
certification matrix and measured `cargo bench --bench query_planner_
bench` numbers.

**RELATIONAL DATABASE PRODUCTION READY = NO.** No executor, SQL
execution, CLI, HTTP API, or frontend exists -- this increment's own
explicit stop condition. Write/Read/Compaction, catalog, row storage,
secondary indexes, the transaction engine, and the SQL parser/binder
all remain independently PRODUCTION READY / PASS, unaffected. Full
account: `PHASE_RELATIONAL_QUERY_PLANNER_INCREMENT8_RESULTS.md`.

---

## 2026-09-24

**Implemented:** Increment 9 -- a production-grade, read-only query
executor (`sql/src/exec/`, new module in the existing `rubixdb-sql`
crate): `Plan/PhysicalPlan -> Execute -> Typed Result` against the real
`TableStore`/`IndexBuilder`/`Transaction` primitives, via one pull-based
`Operator` trait implemented by a struct per plan-node kind (`PkLookup`/
`IndexScan`/`SeqScan`, `Filter`, `Projection`, `Distinct`, `Sort`,
`Limit`, `NestedLoopJoin`). Only `SELECT` executes -- every write-shaped
`Plan` (`Insert`/`Update`/`Delete`/`Ddl`) returns a controlled
`UnsupportedExecution` error, never a silent no-op, per this increment's
own "all writes are outside this increment" scope.

Inspecting (never guessing) the actual planner/storage APIs surfaced two
real, small, necessary primitive gaps, both closed with the smallest
additive fix: `PhysicalAccess` (Increment 8) carried a table's `table_id`
but not its `TableRefId`, insufficient for a self-join or even a bare
predicateless scan to resolve `ColumnRef`s against -- fixed by adding
`table_ref: u32` to every `PhysicalAccess` variant. And the storage
layer had no snapshotted scan-shaped read (`Transaction::get_row` is
snapshot-correct, but nothing scan-shaped was) -- exactly the gap
`IndexBuilder::scan_entries`'s own Increment 5 doc comment had already
named and deferred to "D10's future transaction layer," which now
exists. Closed additively in the core crate: `TableStore::get_row_as_of`/
`scan_table_as_of`/`scan_table_rows_as_of` (the last one genuinely
**lazy**, wrapping the certified Read Engine's own already-lazy
`RangeScanIter` directly -- a bare `SELECT * FROM huge_table` never
materializes the whole table), `IndexBuilder::index_lookup_as_of`/
`index_range_scan_as_of`, and a trivial `Transaction::snapshot_seq()`
getter. `IndexScan` itself stays eagerly bounded rather than lazy (a
stated, honest tradeoff -- `IndexBuilder`'s own internals would need a
larger rework than this increment's evidence justifies), bounded by a
new `ExecLimits::max_index_scan_rows` instead.

Three-valued SQL logic (`NULL`/`AND`/`OR`/`NOT`/`IS [NOT] NULL`) is
implemented directly, never via ordinary two-valued `bool`; `LEFT JOIN`
correctly emits exactly one NULL-extended row per unmatched outer row
(a `RowContext` mechanism using an empty `Row` as the null-extension
placeholder, needing no catalog lookup to know the inner table's column
count); `IndexNestedLoop` rebuilds its inner access fresh per outer row
(never caching one outer row's lookup for another); residual predicates
the planner attaches to an `IndexScan` are always evaluated, never
dropped. A `RowContext`/`Tuple` design lets `Sort`/`Distinct` sit above
`Projection` in Increment 8's own unmodified plan-node order while still
resolving an `ORDER BY` expression outside the `SELECT` list, with zero
planner changes.

**A real bug found and fixed** (the same "found by writing a genuinely
exhaustive test, not by inspection" pattern as Increments 5/6/8):
`ORDER BY ... DESC NULLS LAST` produced `NULL` values first instead of
last -- the sort comparator was reversing the already-absolute `NULLS
FIRST`/`LAST` placement a second time whenever `DESC` was also present.
Found by a test written to cover exactly that combination, fixed, both
directions now regression-tested.

36 new executor tests plus 4 new core-crate regression tests for the
snapshotted primitives. Full regression: 542 `rubixdb` + 30
`rubixdb-api` + 179 `rubixdb-sql` tests, debug and release, all passing;
`wal_tests`/`pathological_recovery_matrix`/`crash_consistency`
unchanged; `src/manifest/`, `src/compaction/`, `src/sstable/`,
`src/wal/`, `api/`, `src/catalog/` completely untouched --
`src/relational/txn.rs`'s only change is one additive, 14-line getter.

`PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md` is the full decision
record, `PHASE_RELATIONAL_QUERY_EXECUTOR_INCREMENT9_RESULTS.md` the
certification matrix and measured `cargo bench --bench query_executor_
bench` numbers (an indexed equality lookup ~940x faster than the
equivalent full scan at 1-in-10,000 selectivity; `LIMIT 10` against
50,000 rows both ~11x faster and, independently, directly proven via
`rows_scanned` metrics to touch only a tiny fraction of the table;
`IndexNestedLoop` ~15x faster than plain `NestedLoop` for a selective
200x200 join).

**RELATIONAL DATABASE PRODUCTION READY = NO.** No CLI, HTTP SQL API, or
frontend SQL console exists, and no write statement executes yet --
this increment's own explicit stop condition. Write/Read/Compaction,
catalog, row storage, secondary indexes, the transaction engine, the
SQL parser/binder, and the query planner all remain independently
PRODUCTION READY / PASS, unaffected. Full account: `PHASE_RELATIONAL_
QUERY_EXECUTOR_INCREMENT9_RESULTS.md`.

## 2026-09-24 (later)

**Implemented:** Increment 10 -- a production-grade write executor
(`sql/src/exec/write.rs`, `sql/src/exec/write/metrics.rs`, new modules
in the existing `rubixdb-sql` crate): `SQL write -> Parser -> AST ->
Binder -> Plan -> Transaction -> Write Executor -> TableStore/
IndexStore -> write_batch -> durable committed state`. `INSERT`,
`UPDATE`, `DELETE`, and the DDL forms the current catalog/index
architecture actually supports (`CREATE SCHEMA`/`TABLE`, `DROP TABLE`,
`CREATE`/`DROP INDEX`) now really execute against real, persisted
state -- never a demo, a wrapper, or a transaction bypass.

`UPDATE`/`DELETE` target-row finding is a two-phase, bounded-memory
design, forced as much by the borrow checker as by the "never an
unbounded affected-row vector" requirement: Phase 1 drives Increment
9's own certified read-access machinery (`PkLookup`/`IndexScan`/
`SeqScan`, unmodified) to collect only matching rows' `PRIMARY KEY`
values -- never full rows -- bounded by a new `ExecLimits::max_dml_
target_rows`; Phase 2 mutates each row in a second pass, re-fetching
fresh for `UPDATE` (never reusing what Phase 1 happened to see). DDL
executes as its own atomic unit, deliberately outside the SQL-level
`Transaction`/snapshot-isolation machinery entirely -- reapplying an
Increment 7 decision rather than re-deciding it.

Inspecting the binder before designing execution (never guessing)
surfaced a real, pre-existing bug: an *omitted* `INSERT` column with a
declared `DEFAULT` always bound to a plain `NULL` literal, structurally
indistinguishable from an explicit `NULL` -- any `DEFAULT`-bearing
column omitted from an `INSERT`'s column list would have silently
stored `NULL` instead of its declared default. Fixed at bind time,
where the ambiguity actually originates.

**A second real bug, this one found by a differential test against an
independent reference model, not by inspection**: `Transaction::
put_row` is a generic upsert-at-key primitive with no notion of "this
key must not already exist." `commit`'s own freshness/`UNIQUE`
validation correctly rejects two *concurrently racing* `INSERT`s of the
same `PRIMARY KEY`, but a plain, *later*, non-overlapping `INSERT`
reusing a key an earlier, already-committed transaction used slipped
through silently and overwrote the existing row -- both transactions'
snapshots agreed, so nothing looked like a conflict. Fixed by having
`execute_insert` check for an existing row via `Transaction::get_row`
(the same snapshot-correct read every other statement already uses,
entirely within the one transaction, never a second, independent
conflict detector) immediately before each row's `put_row`, reported as
the same `SqlError::Conflict` class a concurrent conflict already uses
-- closing the *sequential* gap the existing commit-time freshness
check structurally cannot see, without touching that check's own,
still-sole authority over the concurrent case.

42 new write-executor tests, 1 new binder regression test, plus a real,
cross-process, OS-level crash test (`sql/tests/write_crash_
consistency.rs`, reusing `rubixdb::wal::AbortPoint`/`FileWal::set_
abort_hook` verbatim across 9 real abort points) proving a crash during
`INSERT`'s own `write_batch` call can never leave a table row durable
without its secondary-index entry, or the reverse -- item 30's own
"HARD PRODUCTION GATE." Full regression: 542 `rubixdb` + 30
`rubixdb-api` + 222 `rubixdb-sql` tests, debug and release, all passing;
`wal_tests`/`pathological_recovery_matrix`/`crash_consistency`
unchanged; every protected core-crate path (`src/wal/`, `src/manifest/`,
`src/compaction/`, `src/sstable/`, `api/`, `src/relational/`,
`src/catalog/`) completely untouched -- every change this increment is
confined to `sql/`.

`PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE.md` is the full decision
record, `PHASE_RELATIONAL_WRITE_EXECUTOR_INCREMENT10_RESULTS.md` the
certification matrix and measured `cargo bench --bench write_executor_
bench` numbers -- including two honestly-reported unfavorable findings,
not smoothed over: transaction commit latency grows clearly superlinear
from a 16-row to a 128-row write-set (likely `validate_and_build_ops`'s
own per-row freshness re-read, `O(write-set size)` by construction, not
yet investigated further), and concurrent same-table write throughput
does **not** scale with writer count at all in this configuration --
flat at ~280-300 single-row `INSERT`s/sec whether 1 or 32 threads are
writing, plausibly the per-table epoch write lock plus `GroupCommit`'s
batching window interacting to serialize same-table commits, named as
an open question rather than root-caused or silently tuned away.

**RELATIONAL DATABASE PRODUCTION READY = NO.** No CLI, HTTP SQL API, or
frontend SQL console exists. `GROUP BY`/`HAVING`/aggregates/window
functions/subqueries/CTEs/set operators remain entirely unbound at the
binder. `RETURNING`/`UPSERT`/`ON CONFLICT` are not implemented (no
bound grammar exists for either). `CHECK` constraints are not enforced
(catalog-only metadata, no SQL binding path exists). `CREATE DATABASE`
has no execution primitive and returns a controlled error. Write/Read/
Compaction, catalog, row storage, secondary indexes, the transaction
engine, the SQL parser/binder, the query planner, and the read-only
query executor all remain independently PRODUCTION READY / PASS,
unaffected. Full account: `PHASE_RELATIONAL_WRITE_EXECUTOR_
INCREMENT10_RESULTS.md`.

## 2026-09-29

**Implemented:** Increment 11 -- production `GROUP BY`/`HAVING`/
aggregate execution (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`), threaded
through the full existing pipeline end to end: parser -> internal AST ->
binder -> logical plan -> rule optimizer -> physical plan -> executor ->
transaction/read context -> real relational storage. No second query
engine, no client-side aggregation.

The working tree already held an uncommitted, **non-compiling** partial
pass at aggregate binding from an earlier session (`sql/src/
aggregate.rs`'s contract/state/grouping-key types, `Expr::Aggregate`/
`BoundExprKind::Aggregate`/`AggregateRef`, aggregate-call binding in
`bind/expr.rs`, real `GROUP BY`/`HAVING` AST conversion in
`convert.rs`) -- inspected in full before writing anything, per the
"no guessing" rule. Found and fixed one real bug in it
(`AggregateState::merge`'s `Max` arm wrote through an unbound
identifier -- a compile error, so this code had never actually run)
plus two type mismatches and two missing re-exports. What did not yet
exist, and is this increment's own work: `GROUP BY` binding itself,
select-list group-compatibility validation (item 17's core rule -- a
column not in `GROUP BY` and not inside an aggregate call is rejected,
never silently selected from an arbitrary row of its group), aggregate-
call extraction into `BoundSelect`'s own shared, deduplicated,
positionally-indexed `aggregates` list (`BoundExprKind::Aggregate` nodes
rewritten to `AggregateRef(idx)`), the `Aggregate` logical/physical plan
node (wired through every one of the ~15 previously-exhaustive `match`
sites this newly-added enum variant touched across the planner/
executor/`EXPLAIN`), the `AggregateOp` executor itself, resource limits,
metrics, and all testing/benchmarking/documentation.

Two design decisions worth naming: `HAVING` is represented as an
ordinary `Filter` node placed directly above `Aggregate` -- no second
boolean-logic model, and it makes "`HAVING` evaluates once per group,
never once per input row" a structural fact (`AggregateOp`'s own
`next()` is the only thing a `HAVING` `Filter` can ever pull from) 
rather than something a counter has to separately prove. And
`AggregateOp` retains only one representative input row per group
(discarding every other row immediately after it contributes to
aggregate state, per item 12's "streaming" requirement) plus a small
`Vec<AggregateState>` -- provably sufficient because the binder's own
group-compatibility validation guarantees every legal non-aggregate
expression above `Aggregate` is constant within a group, so any member
row answers it identically.

Correctness was proven by an independent reference aggregation engine
(`sql/src/aggregate_reference_model.rs`, item 51/52 -- never calls the
planner/executor/storage), compared against the real pipeline across a
7-scenario fixed matrix (empty input, duplicates, `NULL` grouping,
mixed-`NULL` values, negative/zero values), a 2,000-distinct-group
high-cardinality case, and 64 `proptest`-generated random tables -- all
matched exactly. 48 new tests total (21 binder, 18 executor, 3
differential/property), plus a new `sql/benches/aggregation_bench.rs`
covering per-function cost, `GROUP BY` cardinality (10/1,000/10,000
groups), composite keys, `HAVING` overhead, `GROUP BY` + `ORDER BY` +
`LIMIT`, and rows/sec scaling (1,000-100,000 rows) -- reported honestly,
including two unexplained findings rather than smoothed over: most
single-aggregate queries measured faster than a plain full-table scan
at the same row count (confounded by result-row-count difference, not
per-row aggregate cost, named as such) and 10-group `GROUP BY` measured
slower than 1,000/10,000-group `GROUP BY` at the same row count
(unexplained, not investigated further this increment).

Full regression: 542 `rubixdb` + 270 `rubixdb-sql` tests, debug,
all passing (two unrelated `tests/group_commit/*` WAL throughput-
threshold tests failed on this run -- `src/wal/group_commit.rs` was not
touched, plausibly a debug-build/host-load artifact against a release-
build target, flagged rather than silently ignored). `cargo fmt`/
`clippy -D warnings` clean. `git diff --stat -- src/` shows exactly one
line changed outside `sql/` (a `pub use` re-export addition in
`src/relational/mod.rs`, no storage semantics touched).

`PHASE_RELATIONAL_AGGREGATION_ARCHITECTURE.md` is the full decision
record; `PHASE_RELATIONAL_AGGREGATION_INCREMENT11_RESULTS.md` the
certification matrix and measured `cargo bench --bench aggregation_
bench` numbers.

One pre-existing, documented, *unchanged* limitation surfaced by this
increment's own transaction test rather than newly introduced by it:
aggregation over a `SeqScan`/`IndexScan` input does not exhibit read-
your-own-writes, because those access paths have always read via
`TableStore::scan_table_rows_as_of(snapshot_seq)` directly rather than
`Transaction::get_row`'s write-set-overlay path -- only `PkLookup` has
ever had that property, in any query, aggregated or not.

**RELATIONAL DATABASE PRODUCTION READY = NO.** Subqueries, CTEs, set
operators, window functions, `ROLLUP`/`CUBE`/`GROUPING SETS`,
`COUNT(DISTINCT ...)`, and every product layer (CLI, HTTP SQL API,
frontend SQL console) remain outside this increment's scope, per its
own stop condition. Write/Read/Compaction, catalog, row storage,
secondary indexes, the transaction engine, the SQL parser/binder, the
query planner, and the query executor (read-only + write) all remain
independently PRODUCTION READY / PASS, unaffected. Full account:
`PHASE_RELATIONAL_AGGREGATION_INCREMENT11_RESULTS.md`.

## 2026-09-29 (later)

**Implemented:** Increment 12 -- exposes the already-certified SQL
engine as a real product: `POST /v1/sql` (the one SQL execution path),
a PostgreSQL-style CLI (`rubixdb-cli`, new workspace member), and a
frontend SQL console, all three terminating at the identical HTTP
handler -- no second parser/binder/planner/executor anywhere.

`rubixdb-api` had zero dependency on `rubixdb-sql` at the start (KV-only
API over `LsmEngine` directly) -- inspected in full before writing
anything, per the "no guessing" rule. Added: `AppState.engine` changed
from bare `LsmEngine` to `Arc<LsmEngine>` (additive; every existing call
site unaffected, verified by grep first); a `TableStore`/`IndexBuilder`/
`TransactionManager`/`CatalogService` alongside the existing engine
handle; a session/transaction registry (`api/src/sql_session.rs`) whose
own core design decision -- sessions exist *only* while an explicit
transaction is open, everything else runs fully stateless autocommit --
keeps the common case free of any new bookkeeping at all; typed
request-parameter/response-value JSON encoding (`bigint`/`decimal`/
`time`/`timestamp` as wire-safe strings, never a lossy JSON number);
`/v1/sql`'s own `Reader`-minimum role gate (a documented, justified
exception to the existing method-based default, since one `POST`
endpoint can carry either a read or a write depending on the SQL text)
deferring the real per-statement decision entirely to `rubixdb_sql::
auth::is_authorized` -- one authorization boundary, never a competing
one; read-only `/v1/catalog/*` metadata routes, added only after
confirming by inspection that `system.*` catalog objects have no SQL
`SELECT` path at all (`crate::bind::scope::resolve_table` only ever
resolves *user* tables); cancellation wired through a `spawn_blocking`
boundary plus a drop-triggered `CancellationToken` cancel and a deadline
backstop, reusing `rubixdb_sql::exec::CancellationToken`/`ExecLimits::
deadline` verbatim -- both already built for exactly this integration.

Found and fixed a real regression during this work, the way item 115/
116 requires: eagerly bootstrapping the catalog in `AppState::new`
injected `system.databases`/`system.schemas` rows into the *same flat
keyspace* `/v1/kv`/`/v1/range` already scan (there is no separate
catalog storage area), breaking two pre-existing, certified KV
integration tests and silently falsifying `/v1/metadata`'s own "no
tables, no schema, no SQL" claim for every deployment. Fixed by making
catalog bootstrap lazy -- deferred to the first actual SQL/catalog
request, cached thereafter -- so a pure-KV deployment's keyspace is
byte-for-byte unaffected by this increment's existence.

The CLI (`rubixdb-cli`, binary `rubixdb`) is a genuinely thin HTTP
client -- no dependency on `rubixdb`/`rubixdb-sql` at all, verified by
its own `Cargo.toml`. Implements the locked `\l \ls \lt \d \di \du
\conninfo \c \help \q` command contract against real `/v1/catalog/*`
data (never a hardcoded example), a real interactive REPL (`rustyline`),
and real `-c`/`-f` script mode sharing one server session across every
statement in a run. `session_id` tracking is ordinary client-side state,
never a second transaction implementation -- the CLI only ever forwards
whatever the server's own response says and renders it. A real deadlock
was found and fixed in the CLI's own test harness (`#[tokio::test]`'s
single-threaded default runtime competing with a blocking subprocess
call for one thread -- diagnosed via `Get-Process` finding two hung
`rubixdb.exe` instances and a locked test binary, fixed by switching to
a multi-threaded runtime), the exact kind of finding item 113 asks to
be proven and fixed rather than silently worked around.

The frontend SQL console is one new page integrated into the existing
React console (every other screen unchanged, verified by re-running the
full pre-existing Playwright suite alongside 3 new real-browser E2E
tests, 21/21 passing together). Typed values render through one shared
formatting rule (mirroring the CLI's own `render.rs`), adversarial row
content renders as inert React text nodes (never `dangerouslySetInner
HTML`, verified against real XSS payloads both at the component level
and through a real browser), and the Cancel button performs a real
`AbortController`/`fetch` abort the server observes as a dropped
connection, not a UI-only "hide the spinner." A real browser E2E run
surfaced two apparent test failures that turned out to be genuine test-
authoring mistakes, not application bugs (the app's own correct
singular "Result (1 row)" text; a `GROUP BY` count that was actually
correct once an earlier `DELETE` in the same test sequence was properly
accounted for) -- both traced to their real cause via the failing
test's own captured page snapshot before either being dismissed or
acted on incorrectly.

84 new tests total, all passing against real components (no mocked
engine/server for primary certification): 40 in `rubixdb-api` (15 unit
+ 25 integration, including a genuine concurrent-transaction test
spawning 12 simultaneous independent sessions via `tokio::spawn` on a
multi-threaded runtime with zero cross-contamination), 25 in
`rubixdb-cli` (15 unit + 10 against the real compiled binary and a real
running server), 19 in the frontend (16 Vitest + 3 real Playwright
browser E2E). Full regression: 542 `rubixdb` + 96 `rubixdb-api` + 25
`rubixdb-cli` tests passing; `cargo fmt`/`clippy -D warnings` clean
across the whole workspace; `git diff --stat -- src/ sql/` empty --
zero lines changed in either the certified engine or the certified SQL
crate, confirmed not assumed. The same `tests/group_commit/*` debug-
build throughput/latency threshold failures already documented as pre-
existing in both prior increments' own results docs recurred here too,
with different specific numbers again, always in a file with zero diff
this increment.

Explicitly not measured this increment, flagged rather than silently
claimed: performance/load testing (no p50/p95/p99/throughput numbers
captured for the API, CLI, or frontend), sustained endurance runs,
memory/handle/thread stability over many session cycles, a dedicated
HTTP/JSON fuzzing sweep, and an HTTP-surface-specific crash-consistency
test (the existing write-executor crash test's own certification is
inherited unchanged, since this increment's SQL layer adds no new
durability mechanism of its own).

**RELATIONAL DATABASE PRODUCTION READY = NO.** Additional SQL language
surface (subqueries, CTEs, set operators, window functions) remains
outside every increment's scope so far, and this increment's own
explicitly unmeasured items above are real, named gaps. Write/Read/
Compaction, catalog, row storage, secondary indexes, the transaction
engine, the SQL parser/binder/planner/optimizer/executor (read + write
+ aggregation), and -- as of this increment -- the product surface
(API/CLI/frontend) built on top of them, all remain independently
PRODUCTION READY / PASS for the specific properties each has actually
been tested against. Full account: `PHASE_RELATIONAL_SQL_API_
INCREMENT12_RESULTS.md`.

## 2026-09-29 (GUI + local instance manager)

Increment 13's mission spec ("final production hardening") assumed a
local instance manager and GUI launcher already existed,
"implemented but not fully certified." A real audit of the repository
(workspace members, `grep` for "gui"/"instance" across `api/`/`cli/`/
`frontend/`, checking for the GUI/instance architecture docs the spec's
own Phase 1 asked to be read first) found none of it existed at all --
four workspace members, a direct-client `rubixdb` binary with no
subcommands, none of `PHASE_RUBIXDB_PRODUCT_ARCHITECTURE.md`/
`PHASE_RUBIXDB_LOCAL_INSTANCE_ARCHITECTURE.md`/`PHASE_RUBIXDB_LOCAL_
SECURITY_ARCHITECTURE.md` on disk. Per explicit user direction, this
was built as its own dedicated, real, production-grade increment
rather than silently folded into "certifying" a product surface that
was never built.

Built: a new `rubixdb-instance` crate (real OS-level ownership via
`fs4`'s `flock`/`LockFileEx`, never a PID file; per-OS app-data
directory resolution with path-traversal-proof naming; a persistent
manifest + a generated high-entropy local credential; loopback-only
collision-safe port binding with no TOCTOU; a real HTTP identity
handshake; a dependency-free cross-platform browser launcher); a new
`rubixdb gui` subcommand that finds/creates the local instance, hosts
the real `rubixdb-api` server in-process on a dedicated thread (no
subprocess, no second SQL engine), serves the real production frontend
build with SPA fallback, and opens the browser; `rubixdb instance
list`/`status` for real introspection; the plain `rubixdb` client role
gained automatic local-instance discovery so it is a complete first-run
entry point on its own. Two small additive `rubixdb-api` changes: `GET
/v1/instance` (new, unauthenticated) and optional frontend static/SPA
serving gated by a new `Config.frontend_dist` field, `None` by default
-- the pre-existing standalone-API deployment shape is byte-for-byte
unchanged, verified by re-running the full pre-existing `api`/`cli`
suites unmodified.

Five real bugs found and fixed while building and testing this: (1) a
`tower-http` `not_found_service` call forced every SPA-fallback
response to HTTP 404 regardless of whether a file was actually served;
(2) a lock-release-on-kill test used an unqualified libtest filter name
and silently never ran the scenario it claimed to; (3) the embedded
server's `TcpListener` was never set non-blocking before handing it to
Tokio, so TCP handshakes completed at the OS level (visible in
`netstat` as `ESTABLISHED`) while the async runtime never actually
served a single request -- found via `netstat`/`curl` against a real
running process, not guessed; (4) a failed `EmbeddedServer::start()`
left an orphaned server thread/engine/socket running because the error
paths never signaled shutdown; (5) the plain CLI client's connection
resolution trusted a stale, unverified `instance.json` left behind by
a since-exited process instead of checking liveness, found when a
cross-process persistence test failed outright -- fixed by routing
every connection through the same handshake-verifying `acquire()` path
`rubixdb gui` uses, never the bare disk-read `discover()`.

Real, process-level testing throughout: 27 unit tests in the new
instance crate including a genuine kill-a-real-child-process lock-
release test; 6 new API tests for the two additive routes; 6 new tests
spawning the **actual compiled** `rubixdb` binary as real racing OS
processes (two `-c` invocations racing an unstarted instance to one
owner, two `gui` invocations racing with the loser correctly attaching
instead of duplicating ownership, a `gui`-owned instance sharing real
data with a separate CLI client, cross-process persistence); a manual
end-to-end smoke test against a real `npm run build` frontend bundle
confirming real index.html/asset/SPA-route/handshake responses. Full
workspace regression: `fmt`/`clippy -D warnings` clean, `cargo check
--workspace --all-targets --all-features` clean, `cargo test
--workspace` all passing except the same pre-existing debug-mode
`group_commit` throughput-threshold flake already documented in both
prior increments' results docs, confirmed unrelated via `git diff
--stat -- src/ sql/` reporting zero lines changed in either certified
path this increment.

Explicitly not claimed: a native desktop GUI (this is a lifecycle
launcher in front of the existing, unchanged browser-based console);
delete-object safety for database/schema/table/index (no such UI
exists anywhere in this product yet); GUI/instance performance, load,
or endurance measurement (deferred to the broader Increment 13
product-hardening pass this increment's own scope explicitly excludes).
Full account: `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INSTANCE_SECURITY.md`, `PHASE_RUBIXDB_GUI_ARCHITECTURE.
md`, `PHASE_RUBIXDB_GUI_INSTANCE_INCREMENT_RESULTS.md`.

Per the user's explicit instruction, this dedicated increment stops
here -- no Router/Replication/Partitioning or other unrelated feature
work follows automatically. Next: the remaining Increment 13 product-
hardening gaps, now against the combined API + CLI + GUI + instance-
management surface.

## 2026-09-29 (Increment 13 hardening: release build + performance baseline)

Began the Increment 13 product-hardening pass proper. Release build
certification: `cargo build --workspace --release` and `cargo test
--release --workspace` both clean except the same pre-existing
`group_commit` debug/release-independent throughput-threshold flake
already documented twice before (re-confirmed in isolation on a fully
idle machine this time -- 11804 vs. a 15000 target, 64416 vs. an 80000
target -- real, reproducible on this hardware, zero diff in the test
file). A real end-to-end production-build smoke test (`rubixdb gui`
serving a real `npm run build` frontend, `rubixdb -c` round-tripping
real SQL, both via the actual compiled release binaries) passed.

Built a new, reusable real benchmark tool
(`api/examples/sql_bench.rs`) and used it to gather a real performance
baseline (`PHASE_RUBIXDB_PERFORMANCE_BASELINE.md`) for PK lookup,
indexed lookup, range scan, full-table count, `GROUP BY`/`HAVING`,
INSERT, UPDATE, and DELETE at concurrency 1-16 (reads) / 1-8 (writes)
against the real release server. Two real bugs found and fixed while
gathering it: (1) the local embedded instance inherited the standalone
API's multi-tenant-deployment rate limit (200rps/burst 400) verbatim,
which throttled its own single legitimate local client under
realistic concurrency -- raised to a documented, still-bounded local
default (2000rps/burst 4000, operator-overridable) after a full
decision record; (2) the benchmark's own INSERT workload never cleared
its table between concurrency levels, causing a self-inflicted
primary-key-collision storm at every level past the first. Real
findings recorded from the corrected data: indexed lookups cost ~10x a
PK lookup (confirmed via a real `EXPLAIN` that the planner correctly
chose `IndexScan`, not a missing-index bug -- the expected cost of an
index-entry-then-row-fetch, a certified Increment 5 characteristic);
write throughput plateaus around 250-320 req/s regardless of
concurrency while latency scales linearly with it, consistent with the
certified single-path group-commit write architecture. Neither is a
regression -- `git diff --stat -- src/` is empty for this work.

Explicitly still open, named rather than assumed: concurrency beyond
16/8, CPU/RSS sampling, sustained endurance, HTTP/JSON fuzzing, a
crash-kill matrix, and GUI/frontend-side performance -- the rest of the
Increment 13 hardening pass.

## 2026-09-29 (Increment 13 hardening: canonical port 302, real crash-kill matrix)

Per explicit user direction (after flagging that ports below 1024 are
OS-privileged on Linux/macOS, confirmed acceptable for a Windows-only
product target), changed the canonical default port from 8080 to 302
everywhere it appears (`instance::port::DEFAULT_API_PORT`, the
standalone API's own env-configured default, the benchmark tool, the
frontend dev-proxy, one still-accurate historical doc reference).
`bind_loopback` now logs a clear diagnostic specifically on a
`PermissionDenied` bind failure rather than silently falling back, so
the privileged-port gap stays visible on a non-Windows host. Verified
end-to-end on the real Windows target: `rubixdb gui` binds
`127.0.0.1:302` and serves real traffic without elevation. Two new
real tests pin the literal value and prove port ownership is correctly
independent from instance-lock ownership (a real unrelated process
squatting on 302 never gets mistaken for a running instance -- the
real owner still acquires the lock normally and falls back to a real
ephemeral port).

Closed the crash-kill matrix gate (Increment 13 Phases Q-W) with four
real tests (`cli/tests/crash_recovery_integration.rs`): the actual
compiled `rubixdb` binary, real `Child::kill()` (an ungraceful
`TerminateProcess`/`SIGKILL`, no destructors, no graceful-shutdown
handler), then a real restart and query through the real product
path -- not the raw engine test harness. Proved: a committed write
survives a real kill; an uncommitted (`BEGIN`, no `COMMIT`) write never
becomes visible after a real kill, through the real HTTP/session path;
committed DDL (schema + table + index together) survives a real kill
with the catalog and index both recovering correctly; and, under
sustained concurrent write load killed at an unpredictable moment, the
recovered table contains no torn/partial rows -- every surviving row's
value matches its id exactly (12/60 attempted rows survived in one
real run, each internally consistent). The core engine's own
crash-consistency certification (`tests/crash_consistency.rs`) is
unchanged -- these are new because nothing before this increment had
proven the same durability guarantees hold when exercised through the
actual product entry point (CLI -> HTTP -> embedded server -> engine)
rather than the engine directly.

Full instance/cli/api regression (fmt, clippy -D warnings, test) clean
throughout. Still explicitly open: higher-concurrency load (32/64),
sustained endurance, resource-trend tracking, HTTP/JSON fuzzing,
expensive-query flood, GUI/frontend performance, and the final
certification documents.

## 2026-09-29 (Increment 13 hardening: real HTTP/JSON/SQL fuzzing and expensive-query-flood evidence)

Closed the HTTP/JSON fuzzing gate (Phases K/L/M) with
`api/tests/api_http_fuzz.rs` -- a real running server (real
`TcpListener`, real `axum::serve`, real `reqwest` client, not the
in-process router shortcut every other API test file uses), fed
hundreds of malformed/adversarial requests across four real tests:
malformed/truncated/random-byte JSON bodies, structurally-valid-but-
semantically-wrong JSON (missing/null/wrong-typed fields, unknown
fields, invalid parameter-type tags), invalid UTF-8 inside a JSON
string field, and 40 random ASCII strings submitted as SQL through the
real parser/binder/planner/executor pipeline; deep/large SQL
expressions (2000-clause `AND` chains, 5000-deep nested parens, a 2MB
string literal, a 100,000-character identifier, a 5000-column
`SELECT`, a 50,000-element parameter array) through the same real
pipeline; malformed/missing/garbage `Authorization` headers and a
forged mismatched `Content-Length`; and repeated abrupt raw-TCP
connection termination mid-request (a genuinely partial HTTP request,
dropped without completing it, exercised directly via a raw socket
rather than `reqwest`, which would otherwise hide the real behavior).
Every case requires either a normal bounded HTTP response or an
acceptable transport-level failure -- a timeout is treated as a hang
and fails the test outright -- and the decisive final check in every
test is that the server still answers a plain `/healthz` correctly
afterward, proving nothing during the run took the process down.

Went further than bare crash-safety for the explicitly expensive
cases (Phase M's own "verify resource protections work," not just
"don't crash"): the deep-parens, huge-literal, and 50,000-parameter
cases assert the request is actively rejected by a real resource limit
(never silently accepted), verified first by hand against a real
running instance (2MB literal -> real `413`; 5000-deep parens -> real
`413 RESOURCE_LIMIT` with the expected "nesting exceeds the configured
recursion limit" detail) before being written into the automated,
repeatable assertion.

No new production dependency -- the randomized inputs come from a
small hand-rolled xorshift64 PRNG in the test file itself, not `rand`.
All 4 new tests pass; full `rubixdb-api` regression (91 tests total
now) re-run clean.

Still explicitly open: higher-concurrency load (32/64), sustained
endurance, resource-trend tracking, GUI/frontend performance, and the
final certification documents.

## 2026-09-29 (Increment 13 hardening: full 1-64 concurrency ladder + real resource sampling)

Extended `api/examples/sql_bench.rs` to the full mission-required
concurrency ladder (1, 2, 4, 8, 16, 32, 64 for reads; 1-32 for writes),
raised read iterations to 1,600/level for statistically meaningful
samples even at c=64, and ran it against a real release
`rubixdb gui --no-browser` while sampling real RSS/handle/thread counts
via `Get-Process` every ~1s throughout. Zero errors across the entire
run (11,200 read + 3,600 write requests).

Found the first rate-limit fix (2000rps/4000burst) was itself still
too low, this time proven rather than guessed: real single-client
PK-lookup throughput alone sustained 13,700-28,793 req/s across the
ladder, comfortably exceeding a 4,000-token burst bucket. Raised again
to 100,000rps/200,000burst, comfortably above the now-actually-
measured ceiling.

Real finding, explicitly not root-caused or silently resolved: PK
lookup throughput keeps climbing cleanly through the whole ladder with
p99 staying under 10ms even at c=64, but every other read workload
(indexed lookup, range scan, count, `GROUP BY`) plateaus in throughput
by c=8-16 and then its own p99/max latency degrades sharply past that
point (range scan p99: 5.31ms at c=1 -> 637.62ms at c=64, a ~120x
increase, while throughput barely moves) -- real evidence of
contention specific to the non-PK read paths under high concurrency,
recorded as an open follow-up rather than either ignored or
"fixed" without the deeper engine-internal analysis this increment's
own scope boundary doesn't yet justify.

Resource trend: RSS climbed from a ~10.0MB idle baseline to a ~46.2MB
peak under c=64 load and visibly came back down to ~24.7MB within ~2
seconds of the load stopping (handles: 118 -> 406 -> 310 over the same
window) -- the shape of bounded, load-proportional use, not monotonic
growth, though this single run wasn't long enough to confirm thread
count (18 -> 202 -> still 199 shortly after) settles all the way back;
that's exactly what the next, longer endurance run is for.

Full regression (fmt, clippy -D warnings, test across instance/cli/api)
clean throughout. Still explicitly open: sustained multi-minute
endurance, GUI/frontend performance, CLI performance/endurance, and
the final certification documents.

## 2026-09-29 (Increment 13 hardening: real sustained endurance run)

Closed a first real endurance pass (Phases E/F/I/J,
`PHASE_RUBIXDB_ENDURANCE.md`) with a new driver
(`api/examples/endurance.rs`): 6 concurrent mixed read/write workers
plus a dedicated session/transaction-cycling worker, against a real
release `rubixdb gui` instance, for 180 real seconds, with real RSS/
handle/thread sampling via `Get-Process` throughout. 97,000+ total
requests; 2,460 errors, every one sampled and confirmed to be the same
real, expected `CONFLICT_ERROR` (genuine snapshot-isolation write-
write conflict detection under deliberately high contention on a
1,000-row shared id space, not a bug). 3,511 real session/transaction
cycles completed (2,340 committed, 1,171 rolled back) including
periodic real snapshot retention held open across concurrent writes
from the other workers -- zero session-related errors, and handle/
thread counts stayed essentially flat across the whole run (120->144
handles, 18->25 threads across 97,000+ requests), real evidence
against a per-request handle/thread/session leak.

RSS climbed from a 9.9MB baseline to a 48.1MB peak and was still at
37.9MB roughly 10s after the run ended -- distinguished (not assumed)
from a leak by correlating it with the table's own real growth (1,000
-> 16,189 rows, a genuine ~16x data-size increase) against the flat
handle/thread counts, which makes a per-request leak specifically
implausible; documented honestly as correlational evidence, not
ownership-traced proof, since no heap profiler was run.

Explicitly still open: a materially longer duration run, GUI/frontend
endurance, CLI endurance, and the final certification documents.

## 2026-09-29 (Increment 13 hardening: CLI performance and handle/thread stability)

Closed CLI performance (Phase AC) and handle/thread stability (Phase
H) with real measurements against the release binary. CLI: a bare
`-c "SELECT 1"` takes 50-75ms end to end (process startup + real
instance-attach handshake dominates -- server-side latency for that
statement is sub-millisecond), while script-mode per-statement cost
(100-statement and 1,000-statement scripts, ~4-6ms/statement) tracks
closely with the same release build's own measured single-client
`INSERT` server latency, meaning the CLI itself adds only 0.5-2ms of
overhead per statement on top of real server cost, not a separate
large cost center.

Handle/thread stability: 50 real, separate `rubixdb -c` process
invocations (each a fresh connect-query-disconnect cycle) moved the
server's own handle/thread counts by +1/+1 total, not per-cycle; a
further 50 real `BEGIN`/`INSERT`/`COMMIT`-or-`ROLLBACK` session cycles
produced zero further change. Correctness verified in the same pass:
committed rows all present, rolled-back rows all absent.

Full findings in `PHASE_RUBIXDB_PERFORMANCE_BASELINE.md` §8-9.
Explicitly still open: a materially longer endurance duration, GUI/
frontend performance and endurance, and the final certification
documents.

## 2026-09-29 (Increment 13 hardening: real GUI/frontend performance)

Closed GUI/frontend execute+render performance (Phases X/Y) with a new
real Playwright suite (`frontend/playwright.gui.config.ts`,
`frontend/e2e-gui/gui_performance.spec.ts`) against the actual product
path -- the real compiled release `rubixdb.exe gui --no-browser`
hosting both the API and the real `npm run build` frontend on one
origin, a real Chromium browser, a real generated local credential.

Page load: `connect()` 305-763ms wall-clock; real Navigation Timing
API numbers `responseEnd` 18-34ms, `load` 48-131ms. The decisive
finding: execute+render time for a real SELECT barely moves (105ms ->
172ms) from a 100-row result to a 10,000-row result -- a 100x increase
in result size produced roughly a 1.6x increase in perceived time,
direct proof that Increment 12's pagination design (render only the
current 200-row page) delivers what it was built for: rendered DOM row
count stayed capped at 200 in every case, confirmed directly via
`.table-wrap tbody tr` counts, never scaling with result size.
Virtualization/incremental rendering were evaluated and not added,
since this evidence shows no bottleneck they would fix.

Seeding 10,000 rows for that last case took 140-177 real seconds --
not the measured metric, but itself further real confirmation (at a
different, larger scale) of the write-path-serialization finding
already documented in `PHASE_RUBIXDB_PERFORMANCE_BASELINE.md`.

A real config gap found and fixed along the way: the new
`e2e-gui/` directory was initially picked up by `vitest` (only
`e2e/` was excluded), causing every Playwright spec's `test.describe`
to collide with Vitest's own test runner -- fixed by extending
`vite.config.ts`'s `test.exclude`; full pre-existing unit suite
(34 tests) re-confirmed passing afterward.

Full results and explicit open items (cross-browser, sustained
execute/clear memory cycling, GUI cancellation/network-failure
timing, the true 100,000-row case) in
`PHASE_RUBIXDB_GUI_PERFORMANCE.md`.

## 2026-09-29 (Increment 13 hardening: real HTTP-disconnect cancellation)

Closed query cancellation via real HTTP disconnect (Phase O) with
`api/tests/api_cancellation.rs`: a real server, a real client that
drops its connection mid-request (`tokio::time::timeout` shorter than
the query's own real completion time, cancelling and dropping the
underlying `reqwest` future -- an actual closed TCP connection, not a
simulated signal), against a genuinely expensive `GROUP BY`/`HAVING`
query made slow via real 24-way concurrent contention (reusing the
tail-latency behavior already measured in
`PHASE_RUBIXDB_PERFORMANCE_BASELINE.md` §7). The cancellation fired
for real after 34.7ms in the recorded run. Decisive proof: the server
answered a brand-new `/healthz` and a fresh `SELECT 1` immediately
afterward (never blocked behind the cancelled or any other in-flight
query), all 24 concurrent background queries eventually completed
rather than hanging, and the underlying data was unaffected
(read-only workload, `COUNT(*)` still exactly 3,000 afterward). Full
regression clean.

## 2026-09-29 (Increment 13 hardening: final certification matrix)

Consolidated every real gate this Increment 13 continuation closed
into four final documents: `PHASE_RUBIXDB_INCREMENT13_PERFORMANCE.md`,
`PHASE_RUBIXDB_INCREMENT13_SECURITY.md`,
`PHASE_RUBIXDB_INCREMENT13_RELIABILITY.md`,
`PHASE_RUBIXDB_INCREMENT13_CERTIFICATION.md`. Each summarizes and
cross-references the detailed evidence documents already produced
(`PHASE_RUBIXDB_PERFORMANCE_BASELINE.md`, `PHASE_RUBIXDB_ENDURANCE.md`,
`PHASE_RUBIXDB_GUI_PERFORMANCE.md`, `PHASE_RUBIXDB_INSTANCE_SECURITY.
md`, plus the real test files themselves) rather than duplicating raw
data.

The certification matrix lists every gate from the mission's own Phase
BJ status list with a real result: `PASS` where real evidence exists
(with `NON-BLOCKING LIMITATION` annotations naming exactly what was
and was not covered within an otherwise-real pass), `NOT APPLICABLE`
for delete-safety (no such UI exists in this product), and `NOT DONE
THIS PASS` -- stated as such, never converted to `PASS` -- for eleven
genuinely unexecuted items: dedicated query-starvation testing, CLI
endurance beyond the handle-stability check already run, GUI
endurance/browser-memory cycling, `CREATE INDEX` mid-backfill crash
testing, commit-acknowledgment-loss as its own scenario, a formal
dependency-advisory scan, simultaneous sustained load across two
instances at once, heap-level ownership tracing for RSS growth, a
materially longer (multi-hour+) endurance duration, cross-browser GUI
timing, and the true 100,000-row GUI case.

**Final production decision: RUBIXDB PRODUCT SURFACE = NOT PRODUCTION
READY**, with the exact blockers being the eleven named `NOT DONE`
items -- not vague, not hidden. This is a materially stronger,
evidence-backed position than existed before this continuation began
(zero GUI/instance-manager code, zero fuzzing, zero crash-kill matrix,
zero endurance evidence, zero measured performance numbers of any
kind), but it is not a `PRODUCTION READY` claim, and none of these four
documents makes one.

## 2026-09-30 (Increment 14 hardening: blockers 1-8, 10-12 closed)

Ten of the Increment 13 continuation's eleven named `NOT DONE THIS
PASS` items closed with real evidence: query starvation (Blocker 1),
CLI endurance (Blocker 2), GUI endurance/browser memory (Blocker 3),
`CREATE INDEX` mid-backfill crash safety (Blocker 4), commit-ack-loss
(Blocker 5), a real `cargo-audit` dependency scan (Blocker 6),
simultaneous multi-instance sustained load (Blocker 7), heap-level
ownership tracing via `dhat` (Blocker 8), cross-browser GUI timing
across real Chromium/Firefox/WebKit (Blocker 10), and the true
100,000-row GUI case (Blocker 11) -- plus an extra delete-safety gate
(Blocker 12) beyond the original eleven. Each has its own
`PHASE_RUBIXDB_INCREMENT14_BLOCKER*.md` evidence document. The eleventh
item, long-duration (multi-hour) endurance, remained open going into
the next session (see below).

Housekeeping found and fixed at the start of the next session: a
generated `dhat-heap.json` profiler dump (572KB) had been accidentally
committed alongside Blocker 8's work -- untracked and gitignored.

## 2026-10-01 (Blocker 9: chained long-duration endurance -- complete)

Full record: `PHASE_RUBIXDB_INCREMENT14_BLOCKER9_LONG_DURATION_
ENDURANCE.md`.

Built `api/examples/long_endurance.rs` (a persistence-aware variant of
the Increment 13 180s `endurance.rs` driver that can resume across a
process restart instead of resetting state, and adds a real `JOIN` to
the operation mix) and `scripts/run_long_endurance_segment.ps1`
(orchestrates one segment: real `rubixdb gui --no-browser`, resource
monitor, workload, final `/v1/status`/`/v1/metrics` capture, hard
process stop). A 20s smoke test caught a real driver bug (a heartbeat
task that overshot its configured deadline by up to 300s per tick)
before committing to the real run -- fixed and reverified.

Three ~115-minute segments (~6912s workload each, ~5.76 cumulative
hours), chained on the *same* persistent instance/data, never reset
between segments, hard-stopped and restarted between segments (doubling
as a real crash-recovery exercise -- instance identity, data, and
correctness all verified intact across both restarts). Table grew
1,000 -> 105,907 (segment 1) -> 156,205 (segment 2) -> 205,987
(segment 3) rows. Resources stayed fully bounded throughout (RSS
sawtoothing ~17-76MB, threads/handles stable, zero storage-pressure
events, automatic compaction observed actually consolidating SSTables
mid-run) -- no resource-growth problem at any point.

**Segment 1's own data directly surfaced the Increment 15 finding
below** (real `indexed_select`/`range_select`/`join` latency degrading
with table growth). Segment 2 continued running the *pre-fix* binary
(already launched before the finding was investigated) and got
dramatically worse as the table grew further -- real `504 TIMEOUT`
failures on `indexed_select` (5) and `range_select` (1), max latencies
up to 32.7 seconds. After Increment 15 landed, the release binaries
were rebuilt and **segment 3 ran the post-fix build**, continuing on
the same growing dataset (156,205 -> 205,987 rows): `range_select`
improved avg 459.96ms -> 2.38ms (~193x) and `join` avg 456.9ms ->
1.98ms (~231x), both with zero errors, despite the table growing a
further 32%. `indexed_select` (a secondary-index path Increment 15
never touched, and was never meant to fix) kept degrading across all
three segments as expected -- flagged as a distinct, still-open
"INDEX READ PERFORMANCE AT SCALE" item for a future increment, not
folded into this one's PASS.

**Blocker 9 verdict: PASS**, on the strength of the post-fix (segment
3) evidence, with the pre-fix segments' real failures kept in the
record rather than discarded.

## 2026-09-30 (Increment 15: PK range scan fix, closing the Blocker 9 ADR)

Segment 1's own data exposed a genuine production-critical finding,
documented on the spot as `PHASE_RUBIXDB_INCREMENT14_BLOCKER9_PK_
RANGE_SCAN_ADR.md`: a bounded PK-range query (`WHERE id >= x AND id <
y`, at most 50 rows) cost ~300x a comparable single-row PK lookup and
grew with total table size (up to 828ms at 105,907 rows) because the
certified planner had no access path for a PK range -- only full-PK
equality (`PkLookup`) or secondary-index access (`IndexScan`) avoided
`SeqScan`'s full-table materialize-then-filter.

Investigated the actual storage/relational layer before writing any
code (per the Increment 15 mandate's own "do not guess" rule): the
certified `LsmEngine::range_scan` primitive already supports an
arbitrary byte-range, already lazy, already snapshot-aware -- no
engine change needed. `src/relational/index_key.rs`'s own module doc
comment already documented `index_id = 0` as "reserved for the table's
own row key," meaning its existing, already-tested `index_scan_range`
prefix/successor byte-range logic could be reused verbatim for PK
ranges rather than reimplemented.

Implemented a new `PhysicalAccess::PkRangeScan` (`sql/src/plan/
access.rs`), planned via a new, deliberately non-shared `candidate_pk_
range_access` function (kept separate from the certified secondary-
index selection code specifically to guarantee zero risk to it -- see
`PHASE_RUBIXDB_INCREMENT15_PK_RANGE_ARCHITECTURE.md` §3.2), backed by
a new `TableStore::scan_table_pk_range_rows_as_of` that narrows the
certified engine call's byte range via the reused `index_scan_range`
helper. `UPDATE`/`DELETE`/`JOIN`/aggregation inherited the fix
automatically through the executor machinery Increment 9 already
certified for the other access paths -- no separate implementation.

Measured (real, in-process, release build, same build for "before" and
"after" via a same-query-different-expression differential technique --
`PHASE_RUBIXDB_INCREMENT15_PK_RANGE_PERFORMANCE.md`): p50 latency for
a 50-row PK range stays 0.09-0.31ms from 1,000 to 100,000 rows (flat),
versus the old `SeqScan` path's 3.9ms-337.6ms for the identical query
on identical data -- a ~1,099x improvement at 100,000 rows. Cost tracks
range width (0.11ms @ 1 row -> 35ms @ 10,000 rows) and is insensitive
to range position within the PK domain, both confirming a genuine
seek-based access path rather than a disguised scan.

New tests: exact multi-column-PK-prefix correctness (the mission's
named landmine -- a partial composite-PK equality bound must return
every row sharing the prefix, not one arbitrary match), MVCC snapshot
isolation, tombstone/reinsert correctness, table isolation, JOIN and
aggregation regression, UPDATE/DELETE exact-target-count regression,
and an extended independent differential/property-testing reference
model (`plan_reference_model.rs`) -- full list in
`PHASE_RUBIXDB_INCREMENT15_PK_RANGE_RESULTS.md` §1-2. Every pre-
existing test in `rubixdb-sql` (287 total after these additions),
`rubixdb` (542), and `rubixdb-api` (45 unit + 40 HTTP/SQL integration)
passes with unmodified expectations. Protected-engine audit clean
(`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`: zero
diff).

One pre-existing, unrelated `cargo clippy --workspace -D warnings`
failure remains, in two Blocker 4/7 test files from before this
session (`cli/tests/multi_instance_sustained_load.rs`, `cli/tests/
index_backfill_crash_integration.rs`) -- flagged, not silently fixed
outside this increment's own mandate; see `PHASE_RUBIXDB_INCREMENT15_
PK_RANGE_RESULTS.md` §4.

**The Increment 14 ADR's "NON-PK READ PERFORMANCE = FAIL" finding is
now closed.** Blocker 9's remaining two endurance segments continue in
parallel; final consolidated Increment 14+15 certification is deferred
until they complete.

## 2026-10-01 (Increment 16: secondary-index read performance + scaling)

Closes the separate open performance finding Blocker 9 recorded for
`indexed_select` (277.8ms -> 783.7ms -> 1,148.1ms as the table grew
~105K -> ~206K rows) that Increment 15 deliberately did not touch. The
Increment 14 Blocker 9 record and the Increment 15 PK-range
certification are unchanged; this is an append-only closure. Full
record: `PHASE_RUBIXDB_INCREMENT16_INDEX_READ_ARCHITECTURE.md`,
`_PERFORMANCE.md`, `_RESULTS.md`.

**Root cause (measured, not guessed):** not the secondary index (index
traversal + entry decode ~0.6us/entry, flat) and not the certified
engine (raw point read ~11us). `IndexBuilder::scan_entries` called
`TableStore::get_row_as_of` once per matched row, and that call
re-resolved table/column catalog metadata (an engine point read + an
engine range scan) every time: 81% of a 1,238ms read at K=10,000 matched
rows / 100K rows. Cost is proportional to matched rows, with a per-row
constant that grew with SSTable count in the old code (~40us/row with
auto-Compaction on, ~100us at 9 SSTables, ~700us at 64). The endurance
predicate matches ~1/10 of the table, so matches grow with the table
*and* the per-row constant grows with the LSM -- hence faster-than-
linear drift. No certified-engine code was changed; no engine ADR needed.

**Fix:** resolve metadata once per scan; fetch each row by the encoded
PK taken directly from the index entry; enforce `max_index_scan_rows`
while collecting (not after). Two label-free counters added
(`index_rows_fetched`, `index_scan_micros_total`).
Candidates measured and rejected: covering index (index +5.3x / ~123%
of the table per index, +232ms per 20K rows of write cost per index),
parallel prefetch (same CPU, no gain once CPU-bound), caches (not
needed once the repeated work is gone).

**Measured (same build, before/after, release):** compaction-on
(production default), 100K rows: K=1,000 56.9 -> 15.3ms, K=10,000
564 -> 145ms; Blocker 9 replay at 105K/155K/206K rows 613/709/1,199ms
-> 194/240/327ms; compaction-off 1M rows K=1,000 734.5 -> 25.6ms
(28.7x). Table-size effect flat 100K -> 1M (K=1,000: 15.3 -> 18.2ms).
32-thread throughput ceiling 2.4x higher, p99 ~2.2x lower; PK reads,
PK range, SeqScan unchanged; JOIN/aggregation over an index predicate
1.7-7x faster; UPDATE/DELETE via index 1.2-1.6x faster.

**Two pre-existing correctness defects found by the new randomized
differential tests (flagged, not silently resolved):**
F-1 (FIXED) an upper-bound-only index range (`a <= x`) returned rows
whose indexed value is NULL (reproduced on unmodified HEAD; fixed in
`candidate_index_access`, regression test added). F-2 (OPEN) a snapshot
transaction older than an index (re)build misses rows when it reads
through that index (reproduced, `#[ignore]`d test; needs a design
decision, recommended as its own increment).

**Tests added:** independent-reference-model differential/property
suite (24 proptest cases + 5x600-step seeds, 37 automatic Compaction
cycles overlapped, snapshot reads, composite index, JOIN/COUNT, exact
UPDATE/DELETE counts, physical index-entry audit: zero stale/orphan/
duplicate), selectivity/result-size sweep, resource-limit boundary test,
bounded-scan relational test, F-1 regression.

**Verification:** `cargo fmt --check` clean; `cargo clippy --workspace
--all-targets --all-features -D warnings` clean (the two previously-known
CLI test clippy failures cleaned up as housekeeping, semantics
unchanged); protected-path audit (`src/wal/`, `src/manifest/`,
`src/sstable/`, `src/compaction/`) zero diff. Full regression is **FAIL
only on pre-existing, unrelated items**: WAL throughput targets M1.2/M1.3
(already documented in `PROCESS.md`), and a pre-existing CLI test defect
(`two_instances_simultaneous_...`, identical failure on HEAD) -- see
`..._RESULTS.md` section 1/4. Not claimed: "secondary indexes are
production-ready". Not measured: >1M rows, process-cold runs, mixed
read+write concurrency. Stopped after Increment 16 per the mandate.

## 2026-10-02 (Increment 17: secondary-index snapshot correctness + cost-based access-path selection)

Closes the two open items Increment 16 recorded: F-2 (a snapshot older than
an index (re)build missed rows through that index) and the absence of a
cost-based index-vs-scan decision. Append-only; the Increment 14/15/16
records are unchanged. Full record:
`PHASE_RUBIXDB_INCREMENT17_INDEX_SNAPSHOT_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT17_COST_MODEL_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT17_PERFORMANCE.md`,
`PHASE_RUBIXDB_INCREMENT17_RESULTS.md`.

**F-2 FIXED.** Reproduced first on unmodified HEAD (730ecca) in five
deterministic scenarios (no sleeps; the "BEGIN during CREATE INDEX" cases
drive the real lifecycle via `create_index` + `recover_incomplete_builds`).
Root cause: backfilled index entries are retroactive derived data stamped
with a later sequence than the rows they describe, while the planner treated
any currently-Ready index as valid for every snapshot. The readiness
sequence is the engine sequence of the Building->Ready catalog write, which
is already a durable MVCC version of the index's catalog row -- so the fix
adds no persisted metadata: an index may serve a read only if its catalog row
read *as of the snapshot* is Ready (`CatalogService::get_index_as_of`,
`IndexBuilder::index_row_usable_at`). Otherwise the executor runs the
identical table scan (the physical plan carries a complete fallback predicate
and, where an eliminated Sort relied on index order, the order columns, so
ordering is re-established with the Sort operator's own comparator). A
mutation test (validity ignoring the snapshot) fails all 10 F-2 tests. The
Increment 16 ignored reproduction is now a permanent regression; the
differential harness's snapshot-retirement workaround is deleted. Only
*missing* rows occur in F-2 (a first-draft "extra rows" claim was corrected).

**Cost-based selection.** Decided at execution (a plan cannot know the
snapshot, parameter values or, for a correlated join, the current outer row)
from the exact match count K (index entries are enumerated key-only,
~0.6us each, before any row is fetched), a table-size estimate with a
rigorous drift bound (`|true-rows| <= drift`, property-tested) learned from
scans/backfills/an on-demand count (0.58us/row), and self-calibrating per-row
costs (EWMA, clamped). Break-even K* = N_hi * seq_ns / index_ns -- no
percentage constant. Conservative by construction (unknown/stale -> index).
Statistics: bounded in-memory (<= 4,096 tables), nothing persisted, write
hook 21-26ns (~850ns with 8 contending threads) vs ~4ms per durable write.

**Measured (same machine, before = Increment 16 tree in a clean worktree).**
57 selectivity points across 4 regimes (10K, 100K compaction on/off, 1M):
the model picked the faster path at 55; the other 2 were dead-even ties
(regret <= 1.01x). Crossover 17%-25% (differs per regime). Always-index was
up to 3.7x slower (100K, 90%) / 2.7x (1M, 50%); Auto within 5% of the oracle.
Selective queries, PK paths, planner (8-23us), concurrency 1-32 threads,
JOIN/aggregation/UPDATE/DELETE: unchanged within noise (an apparent 6% DELETE
gap in back-to-back batches was bisected to machine drift; interleaved A/B:
108.2 vs 107.7ms).

**Tests added:** 8 F-2 scenario tests; F-2 churn property (76 rebuilds, 37
Compactions); every differential step now randomizes the access path;
path-independence property with poisoned statistics (12 fixed seeds + 16
proptest cases, INNER/LEFT JOIN, COUNT); decision tests; drift-bound
property; restart test of the ready-sequence boundary; unit tests for cost
and stats.

**Verification:** fmt and `clippy -D warnings` clean; protected-path audit
(`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`) zero diff.
Full regression is FAIL only on pre-existing, unrelated items re-verified on
the Increment 16 tree: WAL throughput targets M1.2/M1.3 (documented in
PROCESS.md since 2026-09-14) and the debug-only CLI test
`two_instances_simultaneous_...` (tokio blocking-in-async; passes in release).
Not claimed: blanket "secondary indexes production-ready", "cost-based
planner production-ready", or "RubixDB production-ready". Not measured: >1M
rows, process-cold runs, mixed read+write load. Eager index-result
materialization, PK-range-vs-index cost comparison, and read-your-own-writes
on scans (pre-existing) are recorded as separate items. Stopped after
Increment 17 per the mandate.

## 2026-10-02 (Increment 18: unified access path, lazy index fetch, transaction scan semantics)

Closes the four open items Increment 17 recorded. Append-only; Increment
14-17 records are unchanged (where they call a behaviour a "scope boundary" or
"deferred" that this increment resolved, the Increment 18 documents supersede
them). Full record: `PHASE_RUBIXDB_INCREMENT18_ACCESS_PATH_ARCHITECTURE.md`,
`_MATERIALIZATION_ARCHITECTURE.md`, `_TRANSACTION_SCAN_SEMANTICS.md`,
`_PERFORMANCE.md`, `_RESULTS.md`.

**1. PK range vs secondary index -- FIXED.** Baseline grid on the Increment 17
tree: the planner always kept the PK range when it consumed more conjuncts,
wrong by up to 838x (R=50,000, K=10: 193ms vs 0.23ms). The planner now
carries every sargable candidate (PK range + each Ready index, bounded to 3
alternatives) and the executor prices each by its exact row count, racing
resumable key-only cursors in lockstep in cost units (no enumeration repeated;
a losing candidate has done only about the winner's cost in probing; a PK range
needs no counting once every index has lost to the table scan). A first
version introduced 65x regret (it probed the index with a table-scan-sized
budget) and a second 1.75x; the shipped race has worst regret 1.44x at 100K
(<= 1.68x at 1M). Fixed cost of a two-candidate decision ~0.1ms.

**2. Eager index materialization -- fixed (hybrid).** Memory was fine
(~0.7KB/row, bounded); the real problem was latency: `LIMIT 10` cost 90% of
the full query because all K rows were fetched. Entries are still enumerated up
front (key-only, bounded by `max_index_scan_rows`) but rows are fetched on
demand: `LIMIT 10` over 25,000 matches 373 -> 16.2ms (23x). Verified by exact
fetch counts: LIMIT 5 fetches 5; no read-ahead; cancel/deadline stop the fetch;
the transaction stays correct. Writes keep bounded materialization.

**3. Transaction scan semantics -- FIXED (a correctness defect).** Contract
(D10 + transaction ADR section 2: "every read a transaction performs" is local
overlay first, snapshot second) requires scans to see the transaction's own
writes; earlier docs only recorded the exclusion as a scope boundary.
Reproduced on HEAD: scans ignored inserts/updates/deletes, and `UPDATE ...
WHERE a = 1` after two inserts in one transaction hit 3 rows instead of 5.
Fix: `Transaction::overlay_for` (versioned, cached, key-ordered, bounded by
`max_write_set_ops`) merged into every scan in the one shared operator
(PK-ordered merge for lazy scans; entry filtering + lazy extras for index
scans; sort for ordered). Cost O(w) per scan, 0 when the table is not written
(<= 4% to w=100; +0.7ms at w=1,000). Mutation check fails 8 of 9 tests;
randomized property (16x90 + 6x250 steps) with outside writers, index rebuilds
under the open snapshot and 7 Compactions passes.

**4. Statistics across restart -- NOT REQUIRED.** Real engine restart
experiment: the first query that needs a table size pays one exact count
(+68ms at 100K, +440ms at 1M; 0.6us/row, once per table, only above 128
matches), one borderline PK-range+index decision runs ~8% slower until the cost
parameters are re-learned, results and all other decisions identical. No
persistence added.

**Measured side effect:** every fetched row was cloned twice into its row
context; a single-clone path (`RowContext::with_row`) made seq scans 33%
faster (342.6 -> 229.7ms) and PK ranges 14% faster as a general speedup.

**Verification:** fmt and `clippy -D warnings` clean; protected-path audit
(`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`) zero diff.
Full regression is FAIL only on pre-existing, unrelated items re-verified on
the Increment 17 tree: WAL throughput M1.2/M1.3 and the debug-only CLI test
`two_instances_simultaneous_...` (debug 1,106 passed / 3 failed; release 1,107
passed / 2 failed). **Observed, not investigated:** fetch-heavy reads plateau
at ~72-100 op/s from ~4 threads with idle CPU (outside the four items; recorded
with data). Not claimed: production-readiness of the access-path optimizer,
secondary-index executor, transactional scans or statistics subsystem, nor of
RubixDB as a whole. Stopped after Increment 18 per the mandate.

---

## 2026-10-02 -- Final single-node production certification + regression closure

**Outcome: ENGINE-BLOCKED -- RubiXDB is NOT declared production ready.** Every product-surface gate
that could be evidenced is PASS; the certified WAL cannot meet the Phase 1 throughput targets
M1.2 (>=15,000 ops/s) / M1.3 (>=80,000 ops/s) on this hardware. `src/wal/` was not modified; no
threshold changed; no PASS claimed. See `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md` and
`PHASE_RUBIXDB_FINAL_SINGLE_NODE_{ARCHITECTURE,PERFORMANCE,SECURITY,RELIABILITY,CERTIFICATION}.md`.

**Reproduced on HEAD b7f0e8b:** M1.2/M1.3 fail debug+release (isolated release 8.6-11.7k / 63-64k;
5.6-6.0k / 32-39k inside parallel `cargo test`); the debug-only multi-instance CLI test was a
test-harness defect (D-0): `reqwest::blocking` in a tokio test panicked and, with no `Drop`, leaked real
server processes. Fixed without touching assertions; passes debug+release.

**Defects found and fixed (each reproduced -> root-caused -> fixed -> regression-tested):**
D-1 `sql/src/parse.rs`: the pre-parse operator-chain guard counted operators inside string literals
(a 260-row ISO-date INSERT or a 40 KB hyphenated text was refused with 413); now refined with the
parser's own tokenizer behind the unchanged zero-alloc fast path; real chains still rejected.
D-2 `cli/src/render.rs`: sanitizer handled C0/DEL but not C1 (U+009B CSI) despite documenting it.
Frontend: react-router-dom 6 -> 7 closes 2 production npm advisories (reachability: not exploitable,
fixed anyway). New real-browser XSS spec. Post-fix regression: debug 1,113 / 2 failed / 26 ignored,
release 1,113 / 2 / 26; the only failures are the two ENGINE-BLOCKED WAL tests. fmt, clippy -D warnings,
check clean. cargo audit: 0 vulnerabilities (1 yanked-crate warning). cargo-deny absent: no license scan.

**Localized (not changed):** read plateau = CPU-saturated index-entry enumeration + per-row fetch
(~54 us/row), not HTTP/serialization; storage not proven responsible. Write ceiling ~270 commits/s per
table = relational per-table epoch lock held across the WAL fsync (`txn.rs` commit), NOT the engine:
270 -> 1,995 commits/s over 1 -> 16 tables. Snapshot Isolation demonstrated incl. write skew (documented).
Increment 18's idle-CPU plateau was not reproduced.

**Endurance:** prior 5.76 h run predated Increments 16-18, so it was fully re-run (3 x 6,900 s, hard-kill
between segments) on current code: 0 non-Healthy samples, SSTables <= 3, flat handles/threads, exact
row counts (142,467 / 212,689 / 266,363), 0 orphans; 293 snapshot-isolation conflicts (all segment 1),
none after; no timeouts/5xx. Binary predates D-1/D-2 (not on the workload path); developer activity
overlapped the first hour of segment 1.

**Open / flagged:** tracked throwaway credential in `frontend/.e2e-crossbrowser-data/` (commit 2afa0e1);
no license scan; no new heap-ownership profile (no "no leak" claim). Protected paths (`src/wal/`,
`src/manifest/`, `src/sstable/`, `src/compaction/`) zero diff. Stopped per the mandate; subqueries, CTEs,
set operations, window functions, Router, Replication, Partitioning NOT started.

---

## 2026-10-03 -- WAL performance resolution + engine re-certification (branch `wal-batch-buffer-fillq`, uncommitted)

**Outcome: WRITE ENGINE remains ENGINE-BLOCKED on the performance target; the one change made is RE-CERTIFIED for
correctness.** M1.2 (>=15,000) and M1.3 (>=80,000) still FAIL: after the change M1.2 ~11.5k (baseline 10.1-10.5k),
M1.3 ~62.6k (baseline ~61.5-63.4k). Targets were NOT lowered; durability NOT traded. See `PHASE_RUBIXDB_WAL_*.md`.

**Hardware:** both physical disks are SATA SSDs (Disk 0 "SSD 128GB" = E:, Disk 1 "LAPCARE" = C:+D:). **No NVMe** => the NVMe
experiment is OPEN / HARDWARE-GATED; no claim depends on it. Correction to prior practice: the M1 tests write under %TEMP% (C:)
unless TMP/TEMP are overridden; all comparisons here state their disk and use E:\waltmp.

**Findings (measured):** latency-bound cycle (~4.3 ms window + ~4.4 ms fsync + ~0.3 ms), disk only ~6% busy. Parallel flushes on one
SATA device serialize (2 lanes: +15% flush rate at 1.7x per-flush latency); two physical disks scale ~2.2x => sharded WAL has no
benefit on a single device (not implemented). Window sweep: M1.2 best at 2-4 ms, M1.3 best at 8 ms => no single fixed window.
Spin retained (sleep/hybrid: 2-3x CPU at 100 writers, no separable gain). Pipelining rejected (prototype reproduces non-adoption).

**Implemented (stage 1, `src/wal/group_commit.rs`, +192/-2):** count-aware, quiescence-guarded early close of the leader's batch
window (cohort = previous batch size, quiescence = clamp(400 ns x cohort, 100 us, 1 ms), only for cohorts <= 256 -- empirical,
hardware-dependent). Durability/ordering/recovery/format/failure semantics unchanged. M1.2 +13.6%; 2/4/8/16/32/64 writers
+85/+76/+62/+44/+29/+14%, p50 -32..-46% at 2-16 writers; M1.3 neutral; single writer unchanged. Disclosed: 64-writer max/p95 worse,
one unexplained 100-writer p99.9 outlier. **Product-level SQL write throughput unchanged** (relational per-table commit lock).

**Stage 2 NOT implemented -- DECISION REQUIRED:** a leader-written batch buffer (prototype: 17-18k / 93-101k on the same SATA disk)
changes failure semantics (a failed flush must poison the committer; today a failed write fails one caller). See architecture §6.

**Re-certification:** debug and release 1,115 passed / 2 failed (exactly M1.2/M1.3) / 26 ignored; fmt, clippy -D warnings, check
clean; protected paths (`manifest/ sstable/ compaction/`) zero diff. Real process-kill: 110 WAL + 30 engine cycles clean; new
independent ack oracle (`examples/wal_ack_oracle.rs`, self-tested with a mutant) 240 cycles / 1,479,155 acknowledged records, 0
losses, 0 partial groups. Method limit: process kill cannot detect ack-before-fsync (page cache survives); power-loss not testable.
**Open:** stage-2 decision; NVMe; power-loss testing; branch uncommitted/unmerged; kill-during-recovery not targeted.

---

## 2026-10-03 -- WAL production performance optimization (flat-combining group commit), branch `wal-batch-buffer-fillq`

**Outcome:** M1.2 / M1.3 targets are now MET on this SATA hardware in isolated release runs (M1.2 17.1k warm / 18.1k cold,
M1.3 99.0k warm / 100.0k cold; 18 of 18 interleaved runs pass, worst margins +9% / +15%; 3/3 canonical `cargo test --release
--test group_commit -- --test-threads=1`) with the same tests and thresholds. **The WAL is still NOT PRODUCTION READY under
the mandate's Rule 38**: FULL REGRESSION is not clean (the two throughput tests fail under the default concurrent/debug
harness; one pre-existing load-sensitive unit test failed once in debug). A human decision is needed on how the regression
gate should run M1.2/M1.3 (see `PHASE_RUBIXDB_WAL_CERTIFICATION.md` §4). NVMe: HARDWARE UNAVAILABLE.

**Design (src/wal/ only):** `FileWal::append_group` (one write syscall per same-segment run, reusing `encode_wal_frame` and
`SegmentIo::append` rollback) + flat-combining `GroupCommitter::append` (each appender returns only after its OWN frame is
written, so `append`'s contract and the durability/failure model are preserved; per-slot outcomes; panic-safe combiner) +
generalized early window close (cohort/quiescence with a bounded straggler fallback; lone-writer probe restricted to the
lone-writer regime). The earlier leader-written batch buffer was rejected (acknowledges before write). Sharded WAL (one SATA
device serializes flushes), pipelining, write-through/unbuffered I/O (durability unprovable), per-waiter/striped wake and
sleep/hybrid wait were measured and rejected.

**Found by measuring, not assumed:** the lone-writer probe closed ~49% of batches prematurely; a count-only cohort target
regressed the real product multi-table write path by 30% (invisible to M1.x) -> straggler fallback x4 chosen on both workloads;
a suspected "degraded mode" in a soak was a bug in my soak tool (retracted; the mechanism built for it was reverted).
Product SQL write throughput is at parity (+/-5%); the relational per-table commit lock is untouched. p99.9/max are worse at
256-512 writers (disclosed); p50/p95/p99 better everywhere; sustained 64-writer throughput +72% (6.5k -> 11.2k).

**Verification:** fmt/clippy -D warnings/check clean; debug 1,121 passed / 3 failed, release 1,122 / 2 failed (26 ignored);
failures = m1_2/m1_3 under the concurrent/debug harness + a baseline-reproducible (11/12 under CPU load) load-sensitive test.
New tests: differential proptest of `append_group` vs sequential append, failure/rotation/panic/concurrency tests. Real
process-kill: independent ack oracle 300 cycles / 1,471,733 acks / 0 losses (final code; mutant detected 13-14 of 30), 140 WAL +
70 engine kill cycles clean. Soak: 25 min x 64 writers flat (RSS 7 MB, threads 65-68, handles 110-112). Not tested: power loss.
Protected paths (manifest/sstable/compaction/lsm/execution, SQL/API/CLI/GUI): zero diff. Documents: `PHASE_RUBIXDB_WAL_{ARCHITECTURE_OPTIONS,
IMPLEMENTATION,CRASH_RECOVERY,PERFORMANCE_FINAL,CERTIFICATION}.md` (the mandate reused the CRASH_RECOVERY file name; the previous
phase's version is in git history).

## 2026-10-04 -- Production operations + disaster recovery + final single-node hardening (branch `wal-batch-buffer-fillq`)

**Outcome: RUBIXDB = NOT PRODUCTION READY** (see `PHASE_RUBIXDB_FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION.md`). Implemented and measured: snapshot-consistent
logical backup (`RUBXBKUP` v1) and crash-safe restore, logical+physical integrity checker, orphan-data maintenance, operator status
(API/CLI/GUI), graceful stop, bounded HTTP front end, data-format marker + WAL preflight guard, reproducible release procedure.
End-to-end lifecycle on the real binary: ALL PASS. Disk-intact kills: 23,386 acknowledged writes, 0 lost, 0 torn transaction pairs.
Restore of 1 M rows 72-146 s; crash restart 0.52 s. Upgrade from two previous builds PASS.

**Found by measuring and fixed:** startup after a kill during CREATE INDEX (600 K rows) took 21-35 s and `rubixdb gui` gave up at 30 s
(recovery now runs after readiness; the certified Increment 14 test was updated for the contract change and gained a
correct-reads-while-building assertion); the API front end held stalled connections forever (now capped and timed out);
CLI let bidi override characters through; `-f` blocked on devices; no graceful stop existed on Windows.
**Found, NOT fixed (engine boundary, ADR-ENG-OPS-001):** `LsmEngine::open` ignores corrupt WAL segments; MANIFEST has no format version
(mitigated at every product entry point).

**Not production ready because:** M1.3 FAILS intermittently on this SATA hardware (bimodal 61-79 k vs 101-113 k; fails in every full
release run), M1.2 OPEN (one 12.3 k outlier), power loss NOT TESTED, real disk-full NOT TESTED, PITR NOT IMPLEMENTED, downgrade UNSUPPORTED.
Full regression: debug 1,191/0/28 clean; release 1,192/1/26 (m1_3). Protected engine paths: zero diff this phase apart from one corrected WAL unit test.
Documents: `PHASE_RUBIXDB_{PRODUCTION_BASELINE,PRODUCTION_OPERATIONS_ARCHITECTURE,BACKUP_RESTORE_ARCHITECTURE,DISASTER_RECOVERY,INTEGRITY_ARCHITECTURE,
OBSERVABILITY_ARCHITECTURE,MAINTENANCE_ARCHITECTURE,PRODUCTION_OPERATIONS_RESULTS,WAL_CERTIFICATION_CLOSURE,FINAL_SINGLE_NODE_RELEASE,FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION}.md`.

## 2026-10-04 -- WAL M1.2/M1.3 diagnosis + certification reconciliation (master `0da9e14`, documentation only)
Diagnosis (`PHASE_RUBIXDB_WAL_M12_M13_DIAGNOSIS.md`, raw data `scratch/wal_diag/`): current M1.3 58/58 runs >= 80 k (93,533-108,235), historical 61-79 k slow mode NOT reproduced, trigger unidentified; M1.2 37/38 >= 15 k, one 14,618 run (longer fsync, no counter explains); baseline `7b7aaaa` 63-67 k / 9.4-12.1 k same session. Root cause not established; hypotheses H1/H5/H6/H7/H8 rejected for the variation observed, H2/H3/H4/H9 inconclusive.
Reconciliation (`PHASE_RUBIXDB_WAL_M12_M13_CERTIFICATION_FINAL.md`): the only formal rule is the tests' per-invocation assert (15,000 / 80,000); no minimum-of-N/percentile/confidence rule exists. Decision: M1.2 OPEN, M1.3 OPEN (supersedes "FAIL intermittent"; observation preserved), FULL RELEASE REGRESSION FAIL (latest recorded release run: m1_3 48,298; not re-run), WAL CERTIFICATION CLOSURE OPEN, POWER LOSS NOT TESTED, NVMe HARDWARE UNAVAILABLE. Not production ready. No source, test, threshold or WAL change; `docs/PROJECT_STATE.md` and `missions/ACTIVE.md` do not exist in the repo.

## 2026-10-04 -- WAL M1.2/M1.3 certification under maintainer Rule A + workspace M1.3 regression investigation (master `0da9e14`, documentation only)
Rule A (every included run individually >= threshold; no tolerance). Set S = all current-code runs with run-level data (src/wal identical to 3ec5034): M1.2 57 runs, 2 below (12,305; 14,618) -> FAIL; M1.3 95 runs, 8 below (34,059; 35,863; 51,860; 61,195; 68,067; 76,614; 76,737; 78,862) -> FAIL; all 58 diagnosis-session M1.3 runs >= 80 k. Set boundary not defined by Rule A (inclusive set used, nothing excluded). Raw file wal_certify_10runs.txt holds 34-52 k runs the closure addendum did not describe.
Workspace: two full release runs (default TEMP on C:, and E:\waltmp): 1,193/0/26 (clean) and 1,192/1/26 (non-WAL CLI test concurrent_first_run_processes_race_safely_to_one_owner); m1_2/m1_3 passed in both. Standalone TEMP test C: vs E: (6+6): one C: run slow (61,195; device write latency 10.4 ms, fsync stage 9.86 ms). Recorded workspace m1_3 failure root cause: UNRESOLVED (P1 not confirmed, P2 no harness defect, no data for the recorded failures). FULL RELEASE REGRESSION FAIL. POWER LOSS NOT TESTED, NVMe HARDWARE UNAVAILABLE. WAL certification FAIL; NOT PRODUCTION READY. No source, test, threshold or WAL change. Details: second section of PHASE_RUBIXDB_WAL_M12_M13_CERTIFICATION_FINAL.md.

## 2026-10-04 -- Phase 7 security gap closure, Increment A: repository hygiene (SG-1, SG-6 option A, SG-7); uncommitted
SG-1: `git rm -r --cached frontend/.e2e-crossbrowser-data` (5 files staged for deletion), directory added to `frontend/.gitignore`, on-disk directory deleted. Verified the cross-browser spec regenerates a fresh credential (chromium/firefox/webkit 3/3 passed; new key sha-256 prefix e53be8bc8e690c82 != burned f0d12d8727506236) and that the regenerated directory is ignored. New `tests/repo_hygiene.rs` (4 tests: no tracked `credentials.json`; no tracked `*.pem/*.key/*.pfx/*.p12/*.jks`; no tracked file with a 64-hex `admin_key` literal; detector self-test). Negative proof: with a fake credentials.json force-staged, 2 of 4 tests FAIL as intended; staged copy removed afterwards. History NOT rewritten (maintainer decision D-1).
SG-6 A: `scripts/release.ps1` no longer builds `-p rubixdb-api` nor copies `rubixdb-api.exe`; prints "Standalone rubixdb-api is not part of v1. Not certified."; asserts the package and SHA256SUMS contain no `rubixdb-api*` (assertion tripped on a planted file). Package contents verified: rubixdb.exe, frontend-dist, VERSION, SHA256SUMS. No TLS/throttle/compare/key-length work done for the standalone binary (D-2).
SG-7: new `scripts/dependency_gates.ps1` (cargo audit -> cargo deny --workspace --all-features check -> npm audit --package-lock-only --omit=dev from frontend/), called by `release.ps1` before tests; exits 1 on first failure. Success path exit 0. Failure path proven with a scratch deny.toml lacking "MIT": cargo deny exit 4, gate exit 1, real deny.toml untouched. (A first failure attempt was invalid -- `--config` placed after `check`; fixed to precede it.)
Release script run end to end with `-SkipTests` (fmt, clippy, gates, npm ci, frontend build, locked release build, package, packaged smoke: DDL/DML/query/backup/verify/check/frontend/graceful stop): exit 0. The full release regression was skipped because it contains the known intermittent `m1_3` WAL failure (OPEN, unrelated); this is NOT TESTED here, not a pass. Also: `cargo test -p rubixdb-instance` 33/0/1; `cargo test --test repo_hygiene` 4/0; fmt, clippy -D warnings, check --workspace --all-targets --all-features clean. This is not the certification pass (PROMPT 3). No production code, no src/wal|manifest|sstable|compaction change.

## 2026-10-04 -- Phase 7 security gap closure, Increment B: credential at rest (SG-2) and replacement (SG-4); uncommitted
SG-2: new `instance/src/winacl.rs` applies the DACL `D:P(A;;FA;;;SY)(A;;FA;;;OW)` (protected = inheritance disabled; SYSTEM + OWNER RIGHTS full control, nobody else) via `ConvertStringSecurityDescriptorToSecurityDescriptorW` + `GetSecurityDescriptorDacl` + `SetNamedSecurityInfoW`. Choice: `windows-sys` 0.61 (typed signatures) as a `cfg(windows)` dependency -- version 0.61.2 is already in Cargo.lock via tokio and others, so the only lockfile change is one edge (`rubixdb-instance` -> `windows-sys 0.61.2`), no new crate source. windows-sys has no safe wrappers; the sole production `unsafe` block is in `restrict_to_owner_and_system` (SAFETY comment; descriptor freed with LocalFree on every path). `credentials.rs::save` is now: create/truncate staging file -> restrict ACL on the EMPTY file -> write -> fsync -> read-back verify (parses and carries exactly this key) -> rename; any failure removes the staging file and leaves an existing credential untouched. Fail closed: if the ACL cannot be applied nothing is persisted and `acquire` returns the error (instance creation fails). Setting the ACL before the secret is written closes the window in which the key sat in a file with an inherited ACL. Unix 0600 unchanged in effect (now also applied before the write). Platforms with neither mechanism refuse to persist.
SG-4: `rubixdb_instance::rotate_credential(name)` and `rubixdb instance rotate-credential <NAME> --confirm <NAME>` (same confirm discipline as `drop`; the confirmation flag is an addition for destructive-action protection). Holds the instance OS lock for the whole operation (refuses a running instance; two rotations cannot both proceed); generates via the same two-UUIDv4 path; writes via the SG-2 `save`; prints no key, only name, confirmation and the credential file path. Precise semantics (also in `--help`): the instance cannot be running during rotation, so no live server holds the old key; the old key is rejected from the instance's next start; clients still using it (open browser tabs, RUBIXDB_API_KEY in scripts) get 401 and must read the new key from the file. Instance id, manifest/port and data are unchanged. A corrupt existing credential file is repaired by rotation. No overlapping keys, no online rotation, no separate revocation (subsumed, B3).
Tests -- instance crate 46 passed / 0 failed / 1 ignored (was 33): Windows DACL read-back equals `D:PAI(A;;FA;;;SY)(A;;FA;;;OW)` with exactly 2 ACEs and no inherited ACE (`AI` in the read-back is the OS auto-inherited descriptor marker, no ACE carries `ID`); control file in the same dir shows inherited (`ID`) ACEs; ACL-call failure propagates as an error; staging failure persists nothing and keeps the old credential; corrupt or wrong-key staged file is never committed (existing file byte-identical, staging removed); stale partial staging file ignored then replaced; rotate: unknown name, traversal/malformed names (`..`, `.`, `../x`, `a/../../b`, `a/b`, `a\b`, empty, `C:\x`, 65 chars) rejected before touching disk, running instance refused, key replaced with identity/port/manifest kept, corrupt credential repaired, restart after a mid-rotation kill artifact uses the old key, and a DETERMINISTIC race (rotation A paused inside its critical section: rotation B gets `InUse` and changes nothing; A then completes -- exactly one winner). CLI process-level (new `cli/tests/rotate_credential_integration.rs`, real binary, real `gui` server, real HTTP) 6 passed: after rotate + restart the old key is 401 and the new 200, identity and data unchanged, malformed auth (none, `Bearer `, `Bearer`, `Basic`, wrong key) all 401, output contains neither key, file ACL verified with `icacls` (SYSTEM + OWNER RIGHTS only, no `(I)`, no Users/Everyone/Administrators/Authenticated); running instance refused with credential bytes unchanged; unknown/malformed names refused and nothing created; 7 bad confirmation shapes exit 2 and change nothing; 96 real process kills at 1 ms steps (71-73 landed after the rename, 23-25 before; stable over 3 runs) -- credential always whole and valid, restart serves data, clean rotation removes stale staging; 6 concurrent rotate processes: all exit 0 or a clean refusal, >= 1 winner, valid final file. Also green: `gui_instance_integration` 7/7, `instance_drop_integration` 8/8, `repo_hygiene` 4/4, fmt, clippy -D warnings, check --workspace --all-targets --all-features, `cargo build --release --locked -p rubixdb-cli`, dependency gates (exit 0) re-run because a dependency edge changed. Not a hot path: no latency/throughput measurement applicable (cold, once per rotation / instance creation). Protected paths src/wal|manifest|sstable|compaction: zero diff.

## 2026-10-04 -- Phase 7 security gap closure, Increment C: redaction, security events, headers, GUI handoff (SG-3, SG-5, O-2, D-3, D-4); uncommitted
C1/C3 redaction: manual `Debug` for `InstanceCredentials`, `ApiKeyConfig` (key -> `<redacted>`) and `Config` (fixed non-secret subset + `..`, keys shown only as a redacted count, so a future field cannot leak by default). `AcquireOutcome` has no `Debug` at all and the `lib.rs` test already printed only a variant name via `debug_variant`; the `{other:?}` leak described in the mission text does not exist in current source -- the test was extended to assert the credentials it holds render redacted. Workspace audit of every production debug-format site (api/cli/instance src): ONE real leak found and fixed -- `api/src/config.rs` `parse_api_keys` echoed the whole raw `RUBIXDB_API_KEYS` entry (key included) when an entry was malformed or had an unknown role; messages now identify an entry by position/name only (test `parse_errors_never_echo_the_key`). Standalone-only code path, redaction of an error message only -- none of the D-2-excluded controls were added. Informational, not changed: `api/src/sql_params.rs:91` echoes an invalid bigint PARAMETER back to its own caller in a 400 body (the caller's own input, not a credential).
C2 security events (`api/src/security_log.rs`): events are emitted as `tracing` events on target `rubixdb_security` and persisted by `SecurityLogLayer` to `<instance dir>/security.log` as one JSON object per line. Closed event set (D-4): `auth.failure`, `admin.action`, `catalog.create`/`catalog.drop`, `instance.start`/`instance.stop`/`instance.drop`, `credential.replace`. Fields: ts_unix_ms, code, principal, method, route PATTERN, status, outcome (ok|denied|refused|failed), object_kind, object, suppressed. The layer copies ONLY those whitelisted fields (an emitter that attaches an `authorization` or `sql` field has them dropped -- tested); every string is clipped to 128 chars and serialized with serde_json so control characters/newlines are escaped (no line forging). Bounds chosen: auth failures rate-bounded to 1 line per 10 s per process (the count of suppressed failures is carried on the next line); 1 MiB per file x (current + 4 rotated generations) = at most 5 MiB per instance, same for the instances-root log. Logging never fails a request (I/O errors are swallowed and counted). Emit points: auth middleware (method + matched route pattern, never the credential or URI); audit middleware on exactly the routes `classify_for_audit` can match (non-GET/HEAD `/v1/admin/*` and the three REST catalog DELETEs; attached per route so SQL/KV/read paths pay nothing); `POST /v1/sql` for DDL (kind + target identifier taken from the parsed AST -- never the SQL text, parameters or rows; EXPLAIN of DDL is not logged); `host.rs` start (emitted inside the server thread before serving, found by a real-binary test after a first version emitted it after readiness and let a request be logged before the start record) and stop; CLI `drop` and `rotate-credential` (outcome ok|refused|failed; not-found/usage errors write nothing). DEVIATIONS TO NOTE: (1) `instance.drop` is written to `<instances root>/instances-security.log`, because the instance's own directory -- and its `security.log` -- is exactly what `drop` deletes; (2) the embedded-mode `tracing` subscriber installed in `cli/src/host.rs` sinks ONLY the security target; all other `tracing` events stay unsunk on purpose (several carry engine error text or filesystem paths and would interleave with the REPL); `api/src/main.rs` untouched; (3) "credential replacement" and "instance lifecycle: rotate-credential" are one event, `credential.replace`; (4) read-only admin inspection (`GET /v1/admin/*`, polled by the console) is not an "action" and is not logged; a reader hitting an admin route (403) is not logged either (D-4 lists authentication failures only).
C4 headers (SG-5): CSP `default-src 'self'; script-src 'self'; style-src 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` on EVERY response (SPA, static assets, API, 401/404), `Cache-Control: no-store` on `/v1/*` responses only. `style-src` decision, from inspection: the built `dist/index.html` has no inline `<style>`/`<script>`, no `data:` URIs, and the 31 `style={...}` props are applied by React through the CSSOM, which CSP does not restrict -- so NEITHER `'unsafe-inline'` nor `'unsafe-eval'` is present anywhere, and the strict policy was verified by real browsers, not assumed (below). A control test proves the browser really enforces it (an injected inline script does not run and an inline style attribute is refused). Consequence: the console can only talk to its own origin (`connect-src` falls back to `default-src 'self'`); pointing the served console at a different origin is no longer possible (standalone/remote use is out of v1 scope).
C5 GUI handoff (D-3): `rubixdb gui` passes `http://127.0.0.1:<port>/#token=<key>` to the OS browser launcher (both when it starts the instance and when it attaches to a running one); the key is never printed. Only a plain token (`[A-Za-z0-9_-]`, <= 256) is ever embedded. Frontend: `main.tsx` reads and scrubs the fragment ONCE before render via `utils/tokenHandoff.ts` (`history.replaceState`, so no history entry holds the key), then `SessionProvider` verifies it with `/v1/whoami` against `window.location.origin`, stores it in sessionStorage ONLY (setSession(..., false)), and holds the route while verifying (`bootstrapping`) -- this is done at boot rather than "on ConnectPage mount" because an unauthenticated `/` redirects to `/connect` and the redirect drops the fragment before ConnectPage ever mounts. A malformed or rejected token is scrubbed and falls back to the Connect page. "Remember" stays opt-in with an explicit warning label above the checkbox. Residual exposure, accepted under D-3: while the launcher runs, the URL (with the key) is in that process's command line (`cmd /C start "" <url>`), visible to the same OS user who can already read `credentials.json`; a browser may briefly hold it in a session store. The real OS-launcher path (default browser) was NOT exercised end to end.
Tests -- unit: api lib 56 (+10 security_log, config redaction/parse); instance 47; cli bin 20 (+2 handoff URL); frontend vitest 40 (+6 tokenHandoff). API router-level (`api/tests/security_events_and_headers.rs`, real router + real layer + real files) 10: headers on `/`, `/sql`, a static asset, API 401/200, `/healthz`, unmatched `/v1`; auth failure recorded without the presented credential or query; 5,000-failure flood -> exactly 1 line (< 512 B); flood with the rate bound OFF and a 4 KiB cap still bounded (total <= 3 x 4096); admin create/verify/refused-delete/delete logged, inspection GET not, backup name/confirm never logged; reader 403 not an action; REST catalog drop logged with numeric id and refused->ok; SQL DDL by kind+name only, INSERT/SELECT/EXPLAIN not logged, row data/column names/SQL keywords/keys absent; DDL failure and reader denial outcomes; hostile identifier cannot forge a line. Real-binary (`cli/tests/security_log_integration.rs`, real `gui`, `RUST_LOG=trace`) 2: event order start -> ... -> stop, 400-failure flood -> <= 2 lines, `admin.action` for `/v1/admin/check` and `/shutdown`, `catalog.create`/`catalog.drop` for the table, `credential.replace`, `instance.drop` in the root log, refused drop/rotate recorded as `refused`; SECRET-SCAN REGRESSION: neither the old nor the new key, nor any attempted key, nor row data, nor `Bearer` appears in security.log, gui stdout/stderr (both runs), rotate output or any API error body; the live key exists in exactly one file in the instance dir (`credentials.json`) and the replaced key in none. Real browsers (Playwright, real release binary): new `e2e-gui/token_handoff.spec.ts` 6 passed (token authenticates without a paste, fragment gone from URL and history, key in sessionStorage not localStorage, fresh tab needs the paste, bad tokens scrubbed, Remember warning present and above the checkbox and the only route to localStorage, headers present, CSP-enforcement control); ALL 20 existing+new GUI specs passed under the CSP including the 100,000-row, 10,000-row, 50-cycle endurance and 40-cycle session specs; cross-browser spec 3/3 (chromium, firefox, webkit); mock-API suite 23/23 including xss_safety and a11y. The Connect-screen visual baseline was regenerated: the diff is exactly the new warning paragraph (inspected side by side), the stale baseline -- not the test -- was wrong; the original is in git history.
Measurement (hot path touched: every request now passes the header layer; auth failure now emits an event). Release build, through the real router in-process (`api/tests/security_overhead_probe.rs`, 5 runs x 40,000 requests, medians, baseline = `git archive HEAD` built separately, 5 interleaved rounds): BASELINE auth-ok 5.4-5.7 us, auth-fail 3.2-3.4 us; FIRST implementation (header layer + audit layer via `from_fn` / four stacked `Router::layer` wrappers) auth-ok 8.3-9.8 us, auth-fail 5.9-7.0 us (+3-4 us -- each wrapper boxes and clones the inner service); FINAL (audit attached only to the ~10 routes it can match, headers in ONE hand-written non-boxing `tower` layer) auth-ok 6.2-6.6 us, auth-fail 4.0-4.1 us = +0.7-1.1 us and +0.7-0.9 us, controls unchanged. This excludes TCP/HTTP parsing (a real round trip is far larger); not measured: end-to-end HTTP latency/throughput, CPU, RSS (no per-request allocation was added beyond header insertion; log writes are bounded and rate-limited).
Also: fmt, clippy `-D warnings` (one lint fixed: `EmbeddedServer` grew past the large-enum threshold, `instance_name` is now `Box<str>`), `cargo check --workspace --all-targets --all-features`, `cargo build --release --locked -p rubixdb-cli`, dependency gates exit 0 (new dependency EDGES only, no new crate: `rubixdb-cli` -> tracing, tracing-subscriber [registry,std]; `rubixdb-api` -> pin-project-lite), full api / cli / instance suites and repo_hygiene green (the `concurrent_first_run_processes_race_safely_to_one_owner` flake in OPEN_ITEMS did not recur). One release-build attempt hit a transient "Access is denied" on the exe, so a Playwright batch ran against the previous binary; it was rebuilt and every browser result quoted above is from the final binary. Protected paths src/wal|manifest|sstable|compaction: zero diff. No engine change was needed.

## 2026-10-04 -- Frontend shell redesign, Increment 1 of 3 (shell only); uncommitted
Scope: frontend/ only. New left rail (240px, collapsible to 64px, drawer <= 640px), top bar (breadcrumb, one search input, notifications bell, avatar), main content container; every existing route renders inside it; no route renamed, so no redirects were needed. Page content untouched. New: `src/components/Icon.tsx` (inline SVG, no library), `src/assets/rubixdb-logo.png` (crop + downscale of `frontend/rubixdb logo.png`, 728 KB -> 40 KB, emitted as a same-origin file, not a data: URI), `src/vite-env.d.ts`, `e2e-gui/shell.spec.ts` (4 tests). Modified: `AppShell.tsx`, `styles/global.css` (shell section), `styles/tokens.css` (additive spec tokens + dark counterparts). Nav (real pages only): Work with data = Home, SQL Console, Monitoring; Data catalog = Catalog, Governance & security; Manage = Compute, Admin, Snapshots. Omitted entirely (no such feature): Ingestion, Transformation, AI & ML, Apps, Marketplace, Data sharing, Postgres.
Deviations: (1) group header "Horizon Catalog" from the brief is a Snowflake product name, which the same brief forbids copying -> used "Data catalog". (2) The Snapshots page has no slot in the brief's list; kept reachable under Manage as "Snapshots". (3) `--text-muted` (#8b929c, ~3:1 on white) is defined but not used for text, to keep WCAG AA; secondary text uses `--text-secondary`. (4) "+" is a link to the SQL Console (a real action, "New query"); rail search icon focuses the top-bar search; the search box does nothing yet (UI-only, no request); bell panel says "No notifications." (no source exists). (5) Log out moved to the rail profile chip (icon button, accessible name "Log out"); the storage-state badge stays in the top bar.
Test selector changes (labels only, no assertion weakened, no test deleted): link names Dashboard->Home, Data Explorer->Catalog, Health / Storage->Monitoring, Operations->Admin, Settings->Governance & security, Compaction->Compute in e2e/workflow, e2e/production_validation, e2e/visual_regression, e2e-gui/delete_safety, e2e-gui/operations, e2e-gui/gui_session_cycles. Settings visual baseline regenerated after inspecting it (diff = the new shell around unchanged page content).
Results: tsc+build ok; vitest 40/40; mock Playwright suite 23/23 (includes axe-core a11y on every screen, responsive, XSS); real-binary GUI suite 20/20 + new shell spec 4/4 under the real CSP; cross-browser 3/3 (chromium, firefox, webkit). Note: `npm run e2e:a11y` does not exist; the axe-core audit is `e2e/a11y.spec.ts`, run inside `npm run e2e`. Lint: 1 pre-existing error (`process` undefined at e2e-gui/gui_session_cycles.spec.ts:19, same line at HEAD) + 3 pre-existing warnings. Protected paths unchanged by this work. Process note: an accidental `git stash` mid-session was popped immediately; nothing lost, the 5 staged deletions of `frontend/.e2e-crossbrowser-data/**` were re-staged.

## 2026-10-04 -- Frontend shell redesign, Increment 2 of 3 (Home page + RAM card); uncommitted
Scope: frontend/ only. Home (route `/`, h1 now "Home", was "Dashboard"; the existing dashboard cards are kept below the new sections, untouched) gets: Quick actions (New SQL query -> /sql; Create snapshot via the existing snapshot-create mutation, admin only; View backups -> /operations, admin only), Recent items (tabs All/Queries/Snapshots/Backups; columns Title/Type/Viewed/Updated; real data only), Start with a template (6 static SQL snippets that load into the SQL console editor and are NOT executed). RAM usage card at the bottom of the rail (hidden when the rail is collapsed / icon-only). New: `pages/HomeSections.tsx`, `components/RamCard.tsx`, `components/Sparkline.tsx` (inline SVG, no chart library), `utils/ram.ts`, `utils/sessionActivity.ts`, tests `HomeSections.test.tsx`, `sessionActivity.test.ts`, `e2e/home.spec.ts` (9), `e2e-gui/home.spec.ts` (2). Modified: `DashboardPage.tsx`, `AppShell.tsx` (RamCard + clear session activity on unmount), `global.css`, and TWO small hooks in `SqlConsolePage.tsx` (`recordQuery(...)` at the two existing history sites; initial editor text from `takePendingSql()`), needed so Home can show queries and load templates; the page's own behaviour/UI is unchanged and its existing tests pass.
Data decisions: Queries = this session's SQL history, IN MEMORY ONLY (module store; never written to web storage; cleared when the shell unmounts: log out / 401 / reload) because the console never persisted history and SQL text can hold sensitive literals. Snapshots = `GET /v1/snapshots`. Backups = `GET /v1/admin/backups`, admin only, and requested only when `/v1/admin/status` says `backups.configured` (otherwise the endpoint answers 501 and Home would raise a failed request - found by the existing "no failed requests" stability test). Viewed = when a query was last run; Updated = snapshot/backup time; "-" where the console has no such fact. No Row count column (no real data). No Import data card (no import path exists).
RAM card: `/v1/status` and `/v1/metrics` carry NO memory fields (verified against the real binary). The only RSS figure is admin-only `/v1/admin/status` resources.rss_bytes, which has no total, so no percentage can honestly be computed. The card therefore shows "Not available" (no number, no sparkline). It is wired to read optional `memory_used_bytes` / `memory_total_bytes` from the existing status query (same 10 s cadence, no new request); those two field names are PROVISIONAL and tested only with intercepted responses. Pill thresholds Healthy < 75%, Warning < 90%, Critical >= 90% are presentation choices, not an engine contract.
Finding: the template check against the real binary caught that `ORDER BY <aggregate alias>` (e.g. `ORDER BY n`) returns NOT_FOUND in the SQL layer; the template uses `ORDER BY COUNT(*) DESC`, which runs. SQL layer not changed.
Selector/label changes (no assertion weakened): h1 "Dashboard" -> "Home" in e2e/production_validation.spec.ts (also `level: 1`), e2e-gui/shell.spec.ts, a11y.spec label string; Settings visual baseline regenerated (the only difference is the rail's new RAM card). A stale `.e2e-data` directory made the workflow range assertion fail once (accumulated keys from many runs); deleting the disposable directory fixed it.
Results: build ok; vitest 56/56; mock Playwright 32/32 (incl. axe-core on Home and the interacted Home, XSS payload, RAM not-available + figures cases); GUI suite on the real release binary 26/26 (incl. shell 4, home 2: all six templates run, no console/CSP errors); cross-browser 3/3. Lint unchanged: 1 pre-existing error + 3 pre-existing warnings. No runtime dependency added (package.json unchanged); no src/, api/, sql/ change by this work.

## 2026-10-04 -- SQL Console page redesign (design only); uncommitted
Scope: the /sql page only, frontend/ only. New layout: worksheet tab strip (client-side, sessionStorage, 20 max, autosaved) / toolbar (Database, Schema, Role chips; Run, Stop while running; Clear; "..." menu) / editor (line numbers, hand-rolled SQL highlighter, current-line band, Ln:Col + char count, autocommit pill, drag/keyboard resize) / results panel with tabs Results, Query details, History (sticky header, row numbers, type tags, 32px rows, client-side pagination 100/200/500, empty/error/running states) / status bar. No new dependency.
Files: pages/SqlConsolePage.tsx (rewritten), components/console/{SqlEditor,WorksheetTabs,ResultsGrid,OverflowMenu}.tsx, styles/console.css, utils/{sqlHighlight,worksheets,csv}.ts, small additions to components/Icon.tsx (play/stop/more/close), components/Tabs.tsx (optional `label`), utils/sessionActivity.ts (history entries carry ms/rows; clearing also drops worksheet storage), styles/tokens.css (dark values for --success/--warning/--danger only). Tests: utils/consoleUtils.test.ts, e2e/z_sql_console_redesign.spec.ts (11, incl. axe in 5 states, XSS cell + column alias, CSV download, Stop, pagination).
Honest scope decisions: Run all OMITTED (the API accepts exactly one statement: "expected exactly one statement, got 2"); Format OMITTED (no formatter); "Save query" OMITTED (worksheets autosave; the status bar shows the real last-autosave time); right sidebar OMITTED (the shell is single-pane); no ?query-param sync (OPEN); Bytes scanned / server timing NOT SHOWN (API returns neither; "Round-trip time" is the browser-measured request time and is labelled so); Schema chip shows "public" only if the catalog lists it, else a dash (the API has no "current schema"); Database chip is the catalog's database. Stop is a real HTTP abort (existing behaviour). Download results is client-side CSV of the already-fetched rows with formula-neutralised text cells. History tab and Home's Recent items share one in-memory store.
Real findings fixed during the work (axe-core): selected results tab accent-on-grey 4.36:1 -> dark text + accent underline; a tablist may not contain buttons -> the tab "x" is a decorative mouse target, keyboard close = Delete key or "Close worksheet" in the menu; keyword colour uses --accent-hover for AA on the current-line band.
Existing-test updates (no assertion weakened): "Execute" -> "Run" (exact) in 9 specs; sql_console.spec history test opens the History tab first; gui_endurance history check now opens the History tab and counts .history-item (>0 and <=50, stricter); cross_browser and hundred_k scroll `.grid-scroll` instead of `.app-content` (the page no longer scrolls by design); SqlConsolePage.test.tsx mocks extended (session + catalog hooks) and sessionStorage cleared per test; home.spec storage test updated (worksheet text is now deliberately in sessionStorage; Home's history is still never stored). New spec is named z_* so it runs after workflow.spec.ts, whose paginated raw-key lookup is pushed off page one by the 250 rows it inserts.
Results: build ok; vitest 69/69; mock Playwright 43/43; real-binary GUI suite 26/26 (100k rows: 1.2 s execute+render, 200 DOM rows, next-page 105 ms); cross-browser 3/3 (chromium, firefox, webkit); lint unchanged (1 pre-existing error, 3 pre-existing warnings). One gui_endurance run failed a `toBeLessThan` growth bound in the first full GUI run; it passed on 2 reruns and in the final full run (front/back avg heap 10.2 MB / 13.2 MB) - suspected GC-timing noise on a heap bound, not investigated further.

## 2026-10-04 -- Single-file binary: console embedded in rubixdb.exe; default instance returns to port 302; uncommitted
Reported problem: the running binary served the OLD console and sat on port 62238. Causes found: (1) `target/release/frontend-dist/` was a stale packaged copy of an old build, and `frontend_dist::resolve()` preferred a `frontend-dist` folder next to the exe over `frontend/dist`; (2) the `default` instance's persisted `instance.json` held `api_port` 62238 from an earlier run that fell back to a random port while 302 was busy, and a persisted port that can still be bound was reused forever (the code default was already 302).
Changes: `cli/build.rs` embeds `frontend/dist` (no source maps) with `include_bytes!` (no new dependency); `cli/src/embedded_frontend.rs` unpacks it once to `<instances root>/.frontend/<content hash>/` (staging dir + atomic rename; refuses paths that escape; removes other builds' folders only after a day) and `frontend_dist::resolve()` now prefers: `RUBIXDB_FRONTEND_DIST` override, then the embedded console, then on-disk folders (only for a binary built without a frontend). `instance::port::bind_for_existing`: the `default` instance returns to the canonical port when it is free, other instances keep their persisted port (self-heal rewrites instance.json).
Verified with the real release exe copied ALONE into an empty folder with a fresh data root and no env overrides: ready at 127.0.0.1:302, served JS/CSS byte-identical to frontend/dist (sha256), logo 200, /sql SPA deep link 200, /v1/status without key 401, CSP/nosniff headers present; a decoy old `frontend-dist` next to the exe was ignored; an instance seeded with api_port 62238 came back on 302. Unit tests: 3 port tests, 3 extraction tests. The old release exe and `target/release/frontend-dist` were deleted and rebuilt; the old instance (PID 1876) was shut down gracefully through its own admin shutdown call before replacing the exe.
Limits: the exe embeds whatever `frontend/dist` held when cargo compiled it (`scripts/release.ps1` already runs `npm run build` first); a plain `cargo build` without a built frontend embeds nothing and prints a warning. The existing `instance.json` of the user's default instance still says 62238 until that instance is next started, then it is rewritten to 302.

## 2026-10-04 -- Correction to the embedded-console entry: unpack location
The console was first unpacked into `<instances root>/.frontend/<hash>/`. `cli/tests/gui_instance_integration.rs` (`two_concurrent_gui_invocations_never_create_two_owners`, `cli_client_attaches_to_a_running_gui_instance_and_shares_its_data`) correctly failed: it expects the instances root to hold only instance folders (`[".frontend","default"]` vs `["default"]`). Fixed in the product, not the tests: the console is now unpacked to the per-user app data directory, `<app data>/frontend/<hash>/` (`%LOCALAPPDATA%\rubiXDb\frontend\<hash>` on Windows). gui_instance_integration 7/7 afterwards; clippy and fmt clean. `concurrent_first_run_processes_race_safely_to_one_owner` (known flake, OPEN_ITEMS) failed once in the full CLI run and passed 6/6 alone; it uses `-c` client mode and never touches the unpack code.

## 2026-10-04 -- Fix: `concurrent_first_run_processes_race_safely_to_one_owner` flake (and a test-harness hang);
Symptom (OPEN_ITEMS, seen 3 times): one of two racing `rubixdb -c` processes exits 1 with EMPTY stderr. Captured by adding exit status + stdout to the assertion message (strictly more diagnostic, nothing weakened) and stressing the test file with 8 busy processes: 2 failures in 30, each `a: status=1 stdout=OK` -- the first statement ran, the second failed. `ERROR: connection error` goes to STDOUT (script runner), which is why stderr was empty.
Root cause: whichever of the two processes wins ownership hosts the server and shuts it down as soon as its own script ends; the other process, attached to it, then finds nobody listening. A genuine product race, not only a test artefact.
Fix (cli only): `Connection` now re-resolves the instance when a request fails at CONNECT level (`reqwest::Error::is_connect`: no bytes were sent, so no statement can have run and a retry can never run one twice). It either attaches to the new owner or becomes the owner itself (and shuts that server down at exit). Bounded to 3 re-resolves per connection; never while a transaction `session_id` is open (that transaction lived in the vanished server); only for connections that attached to another process's instance. Unit tests (4): vanished owner replaced, including after an earlier success; open transaction never continued on a new server; bound enforced; no re-attach without the flag.
Second, separate defect found while reproducing: `gui_instance_integration` could HANG (one run was stuck 35 minutes; it finished the instant the leftover `rubixdb gui` owner was killed). Tests in that file spawn children with piped stdio from parallel threads; on Windows a child can inherit pipe handles another thread is creating at that moment, so one test's `.output()` waits for EOF on a pipe a long-lived owner of ANOTHER test still holds. Fixed in the harness by creating pipes + spawning under one mutex (`spawn_locked` / `output_locked`); waiting is not serialized. No assertion changed.
Evidence: stress (8 busy processes, 30 iterations of the test file): before 28 pass / 2 fail, after 30 / 0; no hang in either stress run after the harness fix; full CLI suite 3 clean runs in a row (27 + 1 + 10 + 4 + 7 + 1 + 8 + 1 + 6 + 2 tests); clippy -D warnings and fmt clean. Not claimed: the failure rate was ~1 in 15 under load, so 30 clean iterations is strong evidence, not proof. Scope: only the SQL request path (`Connection::execute`) re-attaches; the admin/ops request helpers do not.

## 2026-10-05 -- Phase 2 Increment A: startup configuration and credential validation; uncommitted
Baseline gaps closed: F-04 (embedded credential path bypassed the API key rules), F-05 (`RUBIXDB_LOCAL_RATE_LIMIT_BURST=0` / `RPS<=0` locked out every authenticated request including admin shutdown), F-03 (unparsable `RUBIXDB_LOCAL_RATE_LIMIT_*`, `RUBIXDB_INSTANCE_RETRY_BUDGET_MS`, and an invalid `RUBIXDB_FRONTEND_DIST` silently fell back). Scope chosen by the maintainer from the baseline findings (the baseline carries no REQUIRED AND MISSING column): Increment A only; B (CLI/arg/path validation), C (index-recovery shutdown, /readyz), D (ports/signals/misc) NOT started.
Behaviour now: (1) `instance`: `InstanceCredentials::load` validates the key (16-256 chars, `[A-Za-z0-9_-]`; generated 64-hex keys always pass) and reports `credentials.json: <reason>; replace it with rubixdb instance rotate-credential ...` -- never the key (serde messages can echo a malformed value, so only error class/line/column are shown). Every consumer goes through `load` (start, attach, `instance status/stop`), so an unusable file is refused everywhere and `rotate-credential` (which never loads) repairs it. `host.rs` re-checks the key before serving (defense in depth). (2) `api::config::validate_rate_limit` (finite, 0 < rps <= 1e9, burst >= 1) is used by the standalone loader too. (3) New `cli/src/startup_env.rs` parses `RUBIXDB_LOCAL_RATE_LIMIT_RPS/BURST` strictly (unset or empty = documented default; anything else must be valid). (4) `instance::retry_budget_from_env` parses `RUBIXDB_INSTANCE_RETRY_BUDGET_MS` strictly (integer 0..=600000) inside `acquire`, before the lock attempt (`AcquireError::InvalidConfig`). (5) `frontend_dist::env_override` makes an invalid `RUBIXDB_FRONTEND_DIST` an error. `gui::run` and `resolve_connection` validate the environment BEFORE taking the instance lock, so a bad value creates nothing (no directory, manifest or credential) and leaves no process, lock or socket. `gui --help` documents the variables and their ranges.
Files: instance/src/{credentials.rs,lib.rs}; api/src/config.rs; cli/src/{startup_env.rs (new),frontend_dist.rs,gui.rs,host.rs,main.rs}; cli/tests/startup_config_validation_integration.rs (new). No change to src/ (engine), Cargo.toml or Cargo.lock; port contract, bind address and identity handshake untouched.
Tests added: 5 instance unit tests (key rules; `load` never echoes a key or file content, two tests; rotate repairs an unusable file; an invalid retry budget fails before touching disk), 1 api unit test (rate-limit lockout values), 4 cli unit tests (3 in `startup_env`, 1 for the frontend override check), 5 real-binary integration tests (9 invalid rate-limit values x {`-c`, `gui`} fail early naming the variable and create nothing; valid/empty values accepted; 5 invalid retry budgets; nonexistent and index-less FRONTEND_DIST; 6 unusable credential files x {`-c`, `gui`, `instance status`} refused without leaking the secret, then offline rotation succeeds (ownership released) and the instance starts).
Results: fmt clean; clippy `-D warnings` clean; check clean; debug workspace 1261 passed / 2 failed; release workspace 1262 passed / 3 failed. Failures: `tests/repo_hygiene.rs` x2 (tracked `frontend/.e2e-crossbrowser-data/default/credentials.json`, OPEN_ITEMS 2026-10-04 Increment A; verified identical on a clean HEAD with this change stashed) and, in the release run only, `m1_3_thousand_writers_throughput` (documented intermittent; passes in isolation on clean HEAD and with this change, 2 of 2 and 1 of 1 runs). Real-binary re-run of the baseline probes: empty key / 3-char key / spaced key now exit 1 with the message above, 0 orphans; `RATE_LIMIT_BURST` in {0, abc, -1, 4294967296} and `RPS` in {0, -5, NaN, inf, abc} exit 1, 0 orphans; `RUBIXDB_FRONTEND_DIST` nonexistent / index-less exit 1; `RETRY_BUDGET_MS` abc / -5 / overflow exit 1 in 0.03 s (was 10.7 s, silently).
Measured (startup path gained only env reads; N=20/10, release exe, same harness as the baseline): READY p50 new 0.093 -> 0.076 s, clean restart 0.095 -> 0.064 s (1 K rows) and 0.116 -> 0.131 s (200 K rows), after TerminateProcess 0.077 -> 0.063 s and 0.121 -> 0.118 s; RSS 10.6-13.7 MB, 18 threads, 107-130 handles, 1 socket: all unchanged; 0 leftover processes, 0 listeners on 302, row counts correct in every run. Differences are within the run-to-run spread seen in the baseline (graceful stop p50 0.073-0.109 s, which this increment did not touch); no performance claim is made.
Not tested: Increment B-D items; the interactive TTY prompts; non-Windows; large-state startup.

## 2026-10-05 -- Phase 2 Increment B: argument, instance-name, manifest and path validation; uncommitted
Baseline gaps closed: F-01 (`rubixdb gui` ignored `RUBIXDB_INSTANCE_NAME` although `--help` documents it), F-02 (`gui --instance` with no value silently opened `default`; `--instance --no-browser` created an instance literally named `--no-browser`; unknown flags ignored), F-13 (raw OS text for an unusable root, manifest/credential errors naming no file, `RUBIXDB_API_URL=http://` reporting a request to `http://v1/sql`), F-17 (unvalidated `instance.json` `name`: a name different from the directory broke `instance stop`; control characters were printed by `instance list`). Increments C and D NOT started; F-18 (which hosts `RUBIXDB_API_URL` may name) deliberately left OPEN -- only syntax is checked.
Behaviour now: (1) `gui` argument parser is strict (`parse_args`): only `--instance NAME`, `--no-browser` (and `--help`); a missing or `--`-prefixed NAME, a repeated `--instance`, an unknown option or a stray word is an error. Name precedence is `--instance` > `RUBIXDB_INSTANCE_NAME` (non-empty) > `default`, validated before the lock is taken (nothing created on error). (2) `InstanceManifest::load_for(dir)`: the manifest must parse, its `name` must satisfy the instance-name rule and equal the directory name (ASCII case-insensitive, because NTFS aliases `DEFAULT`/`default`); errors name `<dir>\instance.json` and print the name with Debug escaping. Used by acquire, attach, discover and list; `instance drop` and `rotate-credential` keep the lenient load so a broken instance can still be removed or repaired. (3) Root/instance-directory I/O errors now read `cannot use the instance directory <path>: <os error> (instances live under RUBIXDB_INSTANCES_ROOT if it is set, otherwise under the per-user application data folder)`; same for the instances-root listing. (4) `RUBIXDB_API_URL` is syntax-checked before any key prompt or network attempt: absolute http/https URL, host present, no user info, query or fragment; the value is never echoed (a malformed URL can still contain a password).
Files: instance/src/{manifest.rs,lib.rs}; cli/src/{gui.rs,main.rs}; cli/tests/startup_args_and_manifest_integration.rs (new). No change to src/, Cargo.toml, Cargo.lock; port contract, bind address, handshake untouched.
Tests added: 2 manifest unit tests, 2 instance lib tests (foreign manifest refused but droppable; root error names path and setting), 3 gui unit tests (options, rejections, name precedence), 2 `validate_api_url` unit tests, 5 real-binary integration tests (5 bad `gui` argument vectors fail early and create nothing; env name honoured, flag beats env, invalid env name rejected; 4 bad manifest names x {`-c`, `gui`, `instance status`, `instance list`} refused naming instance.json with no control character, then `drop` succeeds; file-as-root error names setting and path; 5 invalid API URLs fail in < 1 s, name the variable, echo no secret, create nothing).
Results: fmt clean; clippy `-D warnings` clean; check clean; debug workspace 1275 passed / 2 failed; release workspace 1276 passed / 3 failed. Failures are the same pre-existing set as Increment A: `tests/repo_hygiene.rs` x2 (tracked burnt credentials file) and `m1_3_thousand_writers_throughput` (release run). m1_3 verified intermittent and unrelated: on clean HEAD with this change stashed it failed 1 of 4 isolated runs; with the change 1 of 2.
Real-binary rerun of the baseline probes (harness unchanged): P2b env-only now opens the named instance; `--instance` without value / followed by a flag / unknown flag / repeated flag exit 1 with 0 orphans and no directory; manifest with foreign name, `../../x`, empty file, bad UUID exit 1 naming instance.json; root-is-a-file and missing-drive errors name the path and RUBIXDB_INSTANCES_ROOT; `RUBIXDB_API_URL=notaurl` / `http://` exit 1 naming the variable (was 0.03 s / 2.3 s with a transport message).
Measured (startup path gained only argument/manifest checks; same harness, release exe): READY p50 new instance 0.093 (baseline) / 0.076 (A) / 0.081 s (B); clean restart 0.095 / 0.064 / 0.062 s (1 K rows), 0.116 / 0.131 / 0.128 s (200 K); after TerminateProcess 0.077 / 0.063 / 0.063 s and 0.121 / 0.118 / 0.135 s; RSS 10.6-13.6 MB, 18 threads, 107-130 handles unchanged; row counts correct, 0 leftover processes, 0 listeners on 302 in every run. Differences are inside the baseline's run-to-run spread; no performance claim is made.
Not tested: non-Windows; interactive `gui` menu; a manifest edited while its owner is running.

## 2026-10-05 -- Phase 2 Increment C: observable index recovery, honest stop during recovery; F-06 blocked on ADR; uncommitted
Baseline gaps: F-12 (`/readyz` was constant `ready:true`, nothing could say whether the post-start index recovery was still running) and F-06 (graceful stop joins the recovery thread with no bound: 11.0-17.2 s measured at 400,000 rows). Increment D NOT started.
F-12 -- implemented. `ready` keeps its meaning (the product can safely accept normal supported work; true while recovery runs, as `index_backfill_crash_integration` proves); recovery is reported next to it, never folded in, so `/healthz`, `/readyz`, the handshake and the printed "ready at" line cannot disagree. New `api/src/recovery.rs`: `RecoveryState` (`not_started|running|complete|failed`) in `AppState.index_recovery`, and `spawn_index_recovery(&state, on_report)` which sets `running` BEFORE it returns (a client that sees the server ready never reads a stale `not_started`), runs the two recovery passes on the named thread and records `complete`/`failed`. `cli/src/host.rs` and `api/src/main.rs` now share it (their two duplicated `recover_incomplete_index_operations` functions are gone; the same stderr/tracing messages are kept). `GET /readyz` gains `index_recovery`; `POST /v1/admin/shutdown` replies `index_recovery` and `waiting_for_index_recovery`; `rubixdb instance stop` prints "is finishing an interrupted index build before it stops; this can take minutes" and, if its 120 s wait ends first, says the instance is still shutting down and is not stuck (it previously said "did not exit within 120 s"). Additive fields only; the request contract and the frontend are untouched.
F-06 -- NOT implemented, by the architecture change-control rule. Bounding the wait needs cooperative cancellation inside `IndexBuilder::backfill` (`src/relational/index.rs`), which is certified relational behaviour outside this mission's modifiable scope; the alternatives (shut the engine down under a running writer, or exit without `engine.shutdown()` and call it graceful) are unsafe or mislabel a kill as a graceful stop. ADR written: `PHASE_RUBIXDB_LIFECYCLE_ADR_INDEX_RECOVERY_CANCELLATION.md` (ADR-LIFECYCLE-001, PROPOSED: cancellation flag checked per chunk, cancelled index stays `Building`, restart protocol unchanged). F-06 stays OPEN pending explicit authorization; Increment C only makes the wait visible and explained.
Files: api/src/{recovery.rs (new),lib.rs,state.rs,main.rs,routes/health.rs,routes/admin.rs}; cli/src/{host.rs,instance_cmd.rs}; tests api/tests/{api_integration.rs,admin_ops.rs}, cli/tests/index_backfill_crash_integration.rs; the ADR file. No change to src/, Cargo.toml, Cargo.lock, the port contract, bind address or handshake.
Tests added: 1 api unit (state round trip), 1 api integration (`readyz_reports_the_index_recovery_state`: never `not_started` right after the spawn call; `complete` after the join; `ready` true), extra assertions on the existing readyz and shutdown tests (`not_started`, `waiting_for_index_recovery:false`), 2 real-binary integration tests on 200,000 rows killed mid-backfill (`/readyz` is `ready:true` + `running` on the first answer after readiness, never flaps, ends `complete` with the index `ready`; `rubixdb instance stop` during recovery prints the waiting notice, ends "stopped cleanly", the owner exits 0 by itself, next start completes the index).
Results: fmt clean; clippy `-D warnings` clean; check clean; debug workspace 1279 passed / 2 failed; release workspace 1280 passed / 3 failed. Failures are the same pre-existing set as Increments A and B: `tests/repo_hygiene.rs` x2 (tracked burnt credentials file) and `m1_3_thousand_writers_throughput` (release run; documented intermittent, verified failing 1 of 4 on clean HEAD in Increment B).
Measured (same harness, release exe): READY p50 base / B / C: new 0.093 / 0.081 / 0.080 s; clean restart 0.095 / 0.062 / 0.063 s (1 K rows), 0.116 / 0.128 / 0.135 s (200 K); after TerminateProcess 0.077 / 0.063 / 0.062 s, 0.121 / 0.135 / 0.128 s; RSS 10.6-13.6 MB, 18 threads, 107-130 handles unchanged; 0 leftover processes, 0 listeners on 302, counts correct. Index recovery (400,000 rows, 5 runs) and stop during recovery (3 runs) re-measured twice and, because the first re-run looked slower (index ready 14.2-22.1 s vs 11.5-15.7 s in the baseline; the seeding step was also 10% slower), once more in the SAME session against a build of pre-Phase-2 HEAD: HEAD index ready 14.5-16.6 s (median 15.2), stop during recovery 10.8 / 13.1 / 19.1 s; this change 13.7-19.1 s (median 15.3), stop 15.4 / 13.7 / 14.4 s. Same distribution: the difference from the baseline run is host variance, not this change. Stop latency during recovery is NOT improved (F-06 open); ready after kill 0.09-0.17 s, 400,000/400,000 rows, unchanged.
Not tested: stop during recovery at sizes above 400,000 rows; the standalone `rubixdb-api` binary end to end (built and clippy-clean only); non-Windows.

## 2026-10-05 -- Phase 2 Increment C2: cooperative cancellation of startup index recovery (ADR-LIFECYCLE-001, option A); uncommitted
Authorized by the maintainer ("implement option A, not option C") after Increment C wrote the ADR. Closes baseline F-06: graceful stop used to join the index-recovery thread with no bound (11.0-17.2 s at 400,000 rows; 10.8-19.1 s on clean HEAD in the same session).
Engine change (the only one, authorized): `src/relational/index.rs` -- `RecoverySummary`, `recover_incomplete_builds_cancellable` / `recover_incomplete_drops_cancellable` (flag read once per chunk, outside the epoch write lock, before that chunk's writes). A cancelled index stays `Building` / `Dropping` -- not `Failed`, not promoted, catalog row kept -- the state a kill leaves, so the certified "restart, not resume" protocol finishes it at the next start; no new on-disk state. Existing methods keep their signatures and never cancel; client-driven `CREATE INDEX` / `DROP INDEX` are never cancelled. A `cfg(test)` chunk hook makes the cancellation tests deterministic and does not exist in non-test builds. WAL, manifest, SSTable, compaction, `src/error.rs`, `Cargo.toml`, `Cargo.lock`: zero diff.
Lifecycle wiring: `api/src/recovery.rs` adds `RecoveryState::Cancelled` (`/readyz` `index_recovery: "cancelled"`), `IndexRecovery::request_cancel`, `RecoveryReport.cancelled`; cancellation is requested at the START of shutdown (the serve trigger in `cli/src/host.rs` and `api/src/main.rs`, plus the admin shutdown handler), in parallel with the request drain, so the join afterwards waits at most one chunk. The owner prints `index recovery was interrupted by shutdown; unfinished indexes stay Building/Dropping and restart at the next start`; `rubixdb instance stop` says it is interrupting the recovery. No timeout was invented and nothing is terminated abruptly: the thread finishes its current chunk, then the engine shuts down through its normal certified path.
Tests added: engine `relational::index_tests` x4 (cancel before start changes nothing and a later run completes; cancel at the chunk-2 boundary of a 1,200-row table leaves `Building`, writes (+100, -10 rows) continue, the restarted pass yields an index equal to the table; cancel mid drop-sweep (2,500 entries) leaves `Dropping` and the restart removes it; the original methods are never cancelled); api unit (state round trip incl. `Cancelled`, cancel flag); `cli/tests/index_backfill_crash_integration.rs` stop test rewritten for the new contract (stop < 4 s while recovery is `running`, owner exits 0 and reports the interruption, next start finishes the index, lookups correct) -- the earlier assertions (kill recovery, readyz running -> complete) are unchanged.
Results: fmt clean; clippy `-D warnings` clean; check clean; debug workspace 1283 passed / 2 failed; release workspace 1285 passed / 2 failed. The only failures are `tests/repo_hygiene.rs` x2 (tracked burnt credentials file, pre-existing, verified on clean HEAD in Increment A); `m1_3_thousand_writers_throughput` passed in this release run (documented intermittent, fails ~1 in 4 on clean HEAD).
Measured (same harness, release exe, 400,000 rows, killed mid-`CREATE INDEX`, 3 runs): stop requested while recovery runs, request -> process exit **0.11 / 0.12 / 0.11 s**, exit code 0, 0 leftover processes, 0 listeners on 302 (baseline 17.2 / 11.0 / 14.0 s; pre-Phase-2 HEAD same session 10.8 / 13.1 / 19.1 s). The next start restarts the recovery (index `building` right after ready, as designed) and the integration test proves it then completes. Ready after kill 0.09-0.17 s, 400,000/400,000 rows unchanged. Index recovery duration, 10 samples with this change: 14.3-20.5 s; earlier runs of the same engine code 13.7-22.1 s; pre-Phase-2 HEAD (n=5) 14.5-16.6 s; seeding speed also drifted +10% between sessions, so the spread is host variance and no recovery-speed claim is made either way (the added cost is one atomic load per 500 rows). Lifecycle benchmark (N=20/10): READY p50 new 0.093 (baseline) / 0.077 s; clean restart 0.095 / 0.076 s (1 K), 0.116 / 0.139 s (200 K); after TerminateProcess 0.077 / 0.066 s, 0.121 / 0.131 s; graceful stop p50 0.05-0.10 s; RSS 10.6-13.7 MB, 18 threads, 107-130 handles, 1 socket unchanged; 0 leftovers.
Not tested: stop latency above 400,000 rows; repeated cancel/restart cycles; the standalone `rubixdb-api` end to end; non-Windows.

## 2026-10-05 -- Phase 2 Increment D: signals, status accuracy, console staging cleanup, default-port case rule, attach latency; uncommitted
Baseline gaps: F-16 (only Ctrl+C was a graceful stop; Ctrl+Break killed the process abruptly), F-09 (`instance status` said "not running" while a process held the lock; `gui` told operators to delete the lock file), F-14 (a `<hash>.tmp-<pid>` console staging folder left by a killed process was never pruned), F-15 (the "default returns to 302" rule compared the name case-sensitively), F-19 (a failed attach probe cost ~2.1 s on Windows). This completes the maintainer-scoped Phase 2 list (A, B, C, C2, D).
F-16: `api/src/shutdown.rs::wait_for_stop` is now the one stop waiter for `rubixdb gui` and the standalone binary: Ctrl+C, Ctrl+Break, console close, user logoff, system shutdown (Windows), SIGINT/SIGTERM (Unix) and the admin API; it reports which one fired (`rubixdb gui: shutting down (Ctrl+Break)...`). A signal that cannot be registered never resolves (no spurious shutdown). Decision: a launcher that left Ctrl+C disabled is RESPECTED (the inherited "ignore Ctrl+C" attribute is not overridden); those processes are stopped with `rubixdb instance stop`. Console close / logoff / shutdown give the process only a few seconds before the OS ends it (tokio keeps the handler thread parked for them), so a longer stop ends like a kill, which the engine tolerates; those three events cannot be generated here and are NOT TESTED.
F-09: `instance status` reports `locked (a process owns this instance but is not answering: it may be starting, stopping or unresponsive)` when the OS lock is held and nothing answers (and `locked (...the process answering on its port is not it)` for a port taken over by another instance); `not running` only when the lock is free. The `gui` message no longer says to delete the lock file (that does not release the lock and could let a second process open the same data); it says to wait, or run `rubixdb instance stop <name>`, and that the OS releases the lock when the owner exits.
F-14: `embedded_frontend::prune_stale` removes every stale (> 24 h) entry other than the folder named exactly like the current build hash -- including `<hash>.tmp-<pid>` staging folders -- and now also runs on the cached path (previously only after a fresh unpack, so a leaked staging folder was never visited in steady state).
F-15: `port::bind_for_existing` compares the name to `default` ASCII case-insensitively (NTFS aliases `DEFAULT`/`default`).
F-19: the identity handshake bounds the TCP connect at 500 ms (`reqwest connect_timeout`); the outcome for an unreachable port is unchanged (`Unreachable`, retried by the attach loop), a bound listener answers the connect immediately.
Files: api/src/{shutdown.rs,main.rs}; cli/src/{gui.rs,instance_cmd.rs,embedded_frontend.rs}; instance/src/{port.rs,handshake.rs}; cli/tests/lifecycle_status_and_attach_integration.rs (new). No change to src/, Cargo.toml, Cargo.lock, port contract (127.0.0.1:302), bind address or the identity handshake semantics.
Tests added: instance unit (default rule ignores case), cli unit (prune removes stale staging folders of any build, never the current folder), 2 real-binary integration tests (status distinguishes a held-but-silent lock from a free one using a real OS lock; the lock message never advises deleting the lock file and a failed probe takes < 1.5 s while the lock stays held). Real-OS signal evidence (harness, real binary, real Ctrl+Break/Ctrl+C events through the console API, 10 runs each): Ctrl+Break request -> exit 0.0039-0.0053 s, exit code 0, line `shutting down (Ctrl+Break)`, 0 leftover processes, 0 listeners on 302, restart first SQL p50 0.072 s with all 3 acknowledged rows present (previously exit code 0xC000013A, no shutdown line); Ctrl+C 0.0041-0.0059 s, exit 0, `(Ctrl+C)`; a launcher that disabled Ctrl+C: Ctrl+C ignored (respected), `rubixdb instance stop` stopped it cleanly.
Results: fmt clean; clippy `-D warnings` clean; check clean; debug workspace 1287 passed / 2 failed; release workspace 1288 passed / 3 failed. Failures: `tests/repo_hygiene.rs` x2 (tracked burnt credentials file, pre-existing) and, in the release run, `m1_3_thousand_writers_throughput` (documented intermittent, failing ~1 in 4 on clean HEAD).
Measured (same harness, release exe): a single failed attach probe against a held lock nobody answers: `RUBIXDB_INSTANCE_RETRY_BUDGET_MS=0` gui 2.12 -> 0.54 s, cli 2.14 -> 0.55 s; budget 300 ms 2.11 -> 0.55 s; default budget unchanged at ~10.55 s (bounded by the budget, more attempts inside it). `instance status` while locked: now `locked (...)`. `instance stop` against the same silent owner still takes 2.06 s (it uses the SQL client, which has no connect bound; not changed). Lifecycle (N=20/10) READY p50 base / D: new 0.093 / 0.077 s; clean restart 0.095 / 0.062 s (1 K rows), 0.116 / 0.136 s (200 K); after TerminateProcess 0.077 / 0.064 s, 0.121 / 0.128 s; graceful stop p50 0.03-0.10 s; RSS 10.6-13.6 MB, 18 threads, 107-130 handles, 1 socket unchanged; 0 leftovers.
Not tested: console close / logoff / system-shutdown events; a staging folder actually left by a kill mid-unpack (only the prune logic is tested); non-Windows (SIGTERM path compiled only); the standalone `rubixdb-api` signal path end to end.

## 2026-10-06 -- Phase 3: lifecycle re-certification (read-only); uncommitted
Scope (maintainer's choice): re-certify the Phase 1 baseline against the final Phase 2 build, no fixes, no production-readiness claim. Output: `PHASE_RUBIXDB_CONFIGURATION_STARTUP_SHUTDOWN_CERTIFICATION.md`. Certified identity: base commit d80020f plus the uncommitted Phase 2 working tree (tracked-diff sha-256 prefix 649a5522eb33e3b5); rubixdb.exe sha-256 a88a3873f79a6f5a748fb7fb53139490c0d2f580dee63cc0e76cc31bbe7f41b1 (plain `cargo build --release --locked -p rubixdb-cli`; not the `/Brepro` release-script build).
Regression: fmt, clippy -D warnings, check clean; debug workspace 1287 passed / 2 failed; release workspace 1287 passed / 4 failed. Failures: tests/repo_hygiene x2 (tracked burnt credentials file, pre-existing); group_commit m1_3 (documented intermittent, engine); and a NEW intermittent failure of the Increment C2 test `a_graceful_stop_during_recovery_cancels_it_quickly_and_the_next_start_finishes_it` in the final release run only (the first `/readyz` answer lacked `index_recovery`; 9 further isolated/CPU-loaded runs passed; cause not established, hypothesis recorded in the certification document; no test was changed).
Baseline findings: 14 of 19 closed with evidence on the final binary (F-01..F-06, F-09, F-12..F-17, F-19; F-13 and F-19 keep residuals); open: F-07, F-08, F-11, F-18; F-10 not required for v1. Re-run measurements (N=10-20, 100-cycle families, real Ctrl+C/Ctrl+Break events): READY p50 0.063-0.140 s, graceful stop p50 0.072-0.103 s (Ctrl+C/Break 0.004 s), stop during index recovery 0.11 s (was 11-17 s), 0 leftover processes/listeners/files, RSS 10.0-13.9 MB, handles 107-132, 1 socket. Certification matrix: 35 rows, PASS 22, FAIL 1, OPEN 3, NOT TESTED 9. Decision: NOT declared production ready (regression FAIL, F-07/F-08/F-11/F-18 open, power loss, disk full, large-state startup, cold cache, hours-long run, release script/packaged artifact and browser rows NOT TESTED).

## 2026-10-06 -- Full Observability (one shot): sampler, system metrics, time series, diagnostics, CLI; uncommitted
Scope: full observability for the single-node product in one session (mission "Full Observability, one shot"); certifies OBSERVABILITY only. Output: `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` (23 sections, 53-row matrix).
Baseline (before any change, 5 runs, real release binary): idle RSS 14.06 MB, 17 threads, 109 handles; 16 clients read p95 1.819 ms at 14,083 q/s; write p95 53.58 ms at 345 q/s. Baseline FAIL found: the `/v1/metrics` route table was unbounded (3,200 hostile requests: 17 -> 821 keys) because the HTTP method token was part of the key.
Implemented: one `rubixdb-sampler` thread at 1 Hz publishing an immutable `Arc<Snapshot>` per tick (first tick synchronous, `catch_unwind`, `OsProbe` seam, staleness derived from age: stale > 5 s, failed > 10 s); bounded rings 15/240/1,440/168 samples x 16 B x 8 series (240,856 B measured, cap 1 MiB); `GET /v1/metrics/system`, `/v1/metrics/system/timeseries?window=`, `/v1/observability/{sessions,queries,events,version}` (Reader, additive, nulls never zeros); closed route-label set (method set + matched pattern, 128 keys + overflow); counters for auth failures, forbidden, rate limited, admin actions, refused sessions, 5xx, connections, active queries; query registry (200 recent, 2,048 in flight, class labels only, never SQL text); security/operational event rings (2 x 256); health classification once per tick (decision requiring review: failed = not ready > 30 s / StorageFull / lock lost; degraded = StoragePressure / free < 10 %); `rubixdb status --system [--json]`. One engine-crate addition: read-only `LsmEngine::compaction_running()`. Protected paths: zero diff; no dependency added.
Tests: `api/tests/observability.rs` 26 + `cli/tests/observability_integration.rs` 3 (real server / real binary: fields, nulls, p95 under 16 readers, timeseries, bounded lists, no credential, adversarial cardinality, 100 sampler cycles, torn reads, CPU/RSS/disk/panic/wedge injection, health, recovery readiness, rates, query states, kill + restart, two real instances) plus unit tests. Found and fixed while testing: client-disconnect cancellations left no event; one test race (generation read before the fault was cleared).
Results: fmt, clippy `-D warnings`, check clean; debug workspace 1,331 passed / 2 failed; release workspace 1,332 passed / 3 failed. Failures all pre-existing and verified at clean HEAD 8e47379: `repo_hygiene` x2 (tracked credentials file), `m1_3_thousand_writers_throughput` (HEAD 69,557 ops/s; this tree 68,133 / 49,300 / 60,922 in three runs).
Measured: cardinality probe 821 -> 23 route keys; `/v1/metrics/system` p95 0.771 ms (release test) and 1.01-1.04 ms against the real binary with 16 readers (0 errors); overhead idle RSS 14.06 -> 14.53 MB (+0.48 MB, +3.4 %), threads 17 -> 18, handles 109 -> 110, CPU 0.0 s in 60 s; load p95/throughput/CPU within baseline run-to-run range; values cross-validated against psutil/directory walk (RSS 2 %, disk and memory totals equal to the byte, sizes equal, compaction counters equal to the existing endpoint).
Not tested: power loss; non-Windows; standalone `rubixdb-api` end to end; wall-clock 24 h / 7 d soak (25 h injected clock); GUI screens (not built); browser e2e. Device-level disk I/O not implemented (I/O fields are process-level).
Verdict: observability implemented and verified; overall PASS not declared (REGRESSION row FAIL from the pre-existing failures). Uncommitted.

## 2026-10-06 -- Full Observability reconciliation and policy resolution (read-only; no source, test or commit)
Scope: Prompt 1 of the observability closure -- reconcile the already-implemented layer (HEAD `2cbd5a7`, clean tree at start) against source, current tests and fresh measurements; classify the twelve open items; produce the health-policy decision packet; investigate the reported `/v1/metrics/system` tail latency. Nothing was implemented, no production source or test was modified, no commit was made, no regression or soak was run. Output: `PHASE_RUBIXDB_FULL_OBSERVABILITY_ARCHITECTURE.md` (as built) and `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` (matrices, classification A-L, decision packet, latency investigation, verifications, proposals P1-P14, maintainer decisions). `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` was NOT modified (Prompt 2 adds the closure addendum).
Fresh evidence: `api/tests/observability.rs` 26/26, `security_events_and_headers` 10/10, `rubixdb-api --lib` 73 passed 1 ignored, `cli/tests/observability_integration.rs` 3/3 (debug); `cargo fmt --check` clean; `tests/repo_hygiene.rs` 2 of 4 FAIL (tracked burnt `credentials.json`, pre-existing). Binaries: HEAD release build (SHA-256 `5c01ef05...0eb8`, reports `git_revision 2cbd5a7e2b18`) and a baseline build of `8e47379` (observability off); a no-git release build reports `git_revision: null`.
Matrices (three, kept apart): observability implementation 75 rows -- PASS 48, FAIL 4, OPEN 6, NOT REQUIRED 3, NOT TESTED 12, NOT IMPLEMENTED 2; workspace regression 12 rows -- PASS 7, FAIL 4, NOT TESTED 1; whole-product readiness 8 rows -- FAIL 2, OPEN 3, NOT REQUIRED 1, NOT TESTED 2. The certification's 52-of-53 PASS is not reproduced: new rows separate capabilities it folded together.
Findings: (1) FAIL -- a client disconnect while a statement runs inside an explicit transaction leaves a permanent entry in the session-observation map (`SqlSessionRegistry.meta`): 60 of 60 aborted sessions stayed `executing`, `active_sessions` 60 vs `active_transactions` 0, the per-principal cap did not apply; introduced by the observability change; fix documented (P1), not applied. (2) FAIL -- `disk.*_iops` / `*_mb_per_sec` are process-level but unlabelled: on an fsync-heavy write load the data device saw 2.0x the process's write operations and ~177x its megabytes (4,467 ops / 0.241 MB vs 9,043 writes / 42.59 MB). (3) FAIL -- `untracked_active` is a cumulative counter. (4) The health thresholds (30 s / 10 %) exist only in the maintainer's earlier mission text, not in any repository document; an instance on `C:` (9.23 % free) reports `degraded`, on `E:` (10.43 % free) `healthy`; `/readyz.ready` (constant true) and `instance.readiness` (sampler-derived) are two different definitions; the not-ready>30 s rule, the real lock probe and five-panic failure are untested; `errors.*` and two `limits.*` fields have no test. Recommended: Option 3 (split by provenance).
Latency investigation (item B): NOT reproduced. 12,107,104 `/v1/metrics/system` requests in 27 runs (slowest 214.0 ms) and 1,852,006 requests with the original harness shape (slowest 65.7 ms); the reported 387 ms-1.35 s did not occur. Every >50 ms request in the 16-reader runs fell in the first 100 ms of the run (synchronized client start); with staggered pre-connected clients the maximum over 3,731,959 requests was 3.0-18.0 ms. `/healthz`, the baseline binary's SQL path and new-connection tests show the same start-of-run / loopback effects, so none is specific to observability. OS probe calls <= 2.35 ms even under load; sampler snapshot age <= 1,020 ms in 53 observed runs at up to 72 k req/s. 50,000 endpoint calls moved no engine, SQL, WAL or process-I/O counter. Item C (thread/handle variance) explained: tokio blocking pool grows to ~489-530 threads under 16-client SQL load, identically on the baseline binary (not an observability effect).
Not done / not tested: no regression, no soak, no power-loss test, no non-Windows run (WSL2 has no Rust toolchain), no Process Monitor trace, no browser. Raw evidence outside the repository: `E:\rubixdb_recon\` (results, results_extra, results_repro, logs, scripts).

## 2026-10-07 -- Full Observability closure: P0 fix, policy, soak, final certification (Prompt 2; resumed after a stop at a second P0)
Scope: the approved decisions D1-D11 only. Started from HEAD `2cbd5a7` with uncommitted work of an earlier closure attempt that had stopped at a second P0 (readiness flapping under write load); that work was resumed. Protected paths untouched; no dependency added; no engine change.
Implemented: (D2) `SessionLease` + per-principal cap counting executing sessions -- leak reproduced on the Prompt 1 HEAD build (60 of 60 sessions stuck `executing`, `active_sessions` 60 vs `active_transactions` 0) and gone on the final build (0 / 0 / 0); (D1) health Option 3 (`classify_health`), advisory `disk.free_advisory`, three-valued `instance.lock_state`; (D3) `process.*` I/O group, `disk.*` volume-only, `device_*` reserved; (D5, option R2 chosen by the maintainer) `/readyz.ready` unchanged (constant true), `instance.readiness` from the same `sampler::ready()`, no read of `sync_failures()`, new additive `instance.coordinator_state`, ADR-OBS-01 (OPEN); (D6) `last_flush_ms` stays null, contract documented; (D9) sampler start/failed logging; (D11) `docs/PROJECT_STATE.md`, `missions/ACTIVE.md`.
Second P0 (found, then resolved by R2): readiness derived from `sync_failures()` flapped -- `/readyz` false in 374 of 426 polls, `instance.healthy` failed 76 of 85, with 4 writers; final build 0 of 424 / 0 of 84.
Measured: overhead (5 runs x idle/read/write x baseline|Prompt 1|final) meets D7 -- idle RSS +0.48 MB, idle CPU <= 0.026 % of a core, +1 thread, read p95 median 1.471 ms in baseline range 0.926-2.112, write p95 73.18 ms in 72.09-76.40. Soak: real process, 127.5 min (7,652.3 s), 7,561 generations, 0 gaps, 0 regressions, snapshot age <= 1,015 ms, 0 errors; observability-only control RSS 12.08 -> 13.41 MB (not monotonic); threads back to 14 vs 15 after a traffic-free tail, handles 121 vs 112 (FAIL as written; the pre-observability binary retains +14 under the same load, so the engine/runtime, not observability). A first soak attempt was invalid (port collision, harness launch order) and discarded. API compatibility (black box vs certified binary): 0 differences on `/healthz`, `/readyz`, `/v1/status`, `/v1/metrics`, `/v1/admin/status`; `/v1/metrics/system`: 12 approved schema changes (D1, D3, D5).
Regression: fmt, clippy `-D warnings`, check clean; debug 1,346 passed / 2 failed; release 1,347 passed / 3 failed; every failure PRE-EXISTING (`repo_hygiene` x2, intermittent `m1_3`). One failure introduced by this mission's own new in-process test (`sampler_start_stop_100_times_...`, thread-count interference) was found and fixed by moving that check to the real-process CLI suite. One existing assertion changed because D1 reverses the 10 % rule (documented in the certification closure section 24.10).
Final matrices: A observability implementation 83 rows -- PASS 63, FAIL 4, OPEN 1, NOT REQUIRED 5, NOT TESTED 9, NOT IMPLEMENTED 1; B workspace regression 14 rows -- PASS 10, FAIL 4, OPEN 0, NOT REQUIRED 0, NOT TESTED 0, NOT IMPLEMENTED 0; C whole product 9 rows -- PASS 1, FAIL 3, OPEN 2, NOT REQUIRED 1, NOT TESTED 2, NOT IMPLEMENTED 0. OBSERVABILITY IMPLEMENTATION mixed; OBSERVABILITY CERTIFICATION not PASS; WORKSPACE REGRESSION FAIL (pre-existing); WHOLE-PRODUCT PRODUCTION READY NOT DECLARED.
Found, not fixed (outside the approved items): `-dirty` revision not applied to a modified-tree build (A10); `untracked_active` cumulative (A32); `errors.wal_sync_failures` and the certified `/v1/admin/status` `wal.poisoned` show a phantom value under write load (A81, C09); handle/RSS retention after load is engine-side (A83). Raw evidence outside the repository: `E:\rubixdb_closure\` (`final\`, `overhead_final\`, `scripts\`), `D:\rubixdb_soak\soak_log.jsonl`, `D:\rubixdb_soak_ctl\`.

## 2026-10-07 -- Full Observability follow-up: binary identity, small fixes, poison signals, D8 amendment, RSS classification
Scope: observability only; `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs` untouched (diff against `a3540ab` empty). The full workspace regression and the 2 h soak were not re-run (instructed). Two commits: `061e058` (code, tests, ADR-OBS-02, ADR-OBS-03) and a documentation commit (hash in the return report). Full record: `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` section 25.
Implemented: (1) binary identity -- release binary force-rebuilt from the clean committed tree `a3540ab` (reports `a3540ab40e64`, sha256 `15d80bd6...`), 5-run overhead re-measured against it; section 25.1 states which binary produced which evidence and notes that the 2 h soak ran on `rubixdb_final2.exe` (source later committed as `a3540ab`; reported revision `2cbd5a7e2b18`, a divergence). (2) `untracked_active` is a gauge (unit test: 20 rounds that start and end at zero in-flight must read 0); `api/build.rs` re-runs for HEAD / ref / index / any tracked file and appends `-dirty` for any tracked modification (modified tree `-dirty`, clean tree bare SHA, documentation-only edit and its revert both checked on real binaries; test compares the embedded revision with `git status`). (3) `/v1/admin/status` `wal.poisoned` reads `GroupCommitter::is_poisoned()` through a read-only pass-through (`src/execution/batch_coordinator.rs`, `src/lsm/mod.rs`, 41 lines, none in `src/wal/`); `errors.wal_sync_failures` is `null` (ADR-OBS-02, ADR-OBS-03). (4) D8 handle criterion amended to "handles after soak <= the pre-observability baseline's under the same workload"; same-workload 30-minute soak on both binaries: +10 vs +10 handles (absolute 122 vs 121).
Measured: overhead on the clean rebuild -- idle RSS +0.47 MB, threads +1, write p95 median 75.53 ms in range; read p95 median 1.674 ms was outside the same-session baseline range (0.937-1.457) but inside the union (0.926-2.112), and a three-way same-session run put it inside (1.315 vs baseline 1.436, final2 1.243); 120 s idle-CPU windows read 0.130 / 0.039 / 0.104 % (baseline 0.065 / 0.026 / 0.078) and two 600 s windows read 0.0000 / 0.0000 % (baseline 0.0026 / 0.0026). `wal.poisoned` true in 124 of 147 real-process polls before, 0 of 142 after; `errors.wal_sync_failures` non-null 147 of 147 before, 0 of 142 after (key present). In-process test: 120 polls under 4 writers all false, then a real injected fsync failure makes it true and keeps it true; with the old derivation the test fails (99 of 120). 30-minute soak (2,012 s, 32 records): RSS saw-tooths with the write/flush cycle (18-31 MB), is flat while ring occupancy rises (read phase +0.11 MB), falls 10+ MB while occupancy only rises, and the pre-observability binary does the same -> engine / runtime, not observability.
Verified: fmt, clippy `-D warnings`; `rubixdb-api` tests (debug; observability 36/36; lib 78), release observability suite 3 x 36/36, CLI `observability_integration` 6/6, `rubixdb --lib batch_coordinator` 18/18.
Matrix (section 25.11): A 84 rows -- PASS 67, FAIL 0, OPEN 2, NOT REQUIRED 6, NOT TESTED 9, NOT IMPLEMENTED 0; B 16 rows -- PASS 11, FAIL 4, NOT TESTED 1; C 9 rows -- PASS 2, FAIL 2, OPEN 2, NOT REQUIRED 1, NOT TESTED 2. Observability certification: not PASS. Whole product: not declared production ready.
Found, not fixed: a really poisoned committer still reads `/readyz ready: true`, `instance.healthy: healthy`, `coordinator_state: alive` (A80 OPEN, ADR-OBS-01); `wal.sync_failures` on `/v1/admin/status` keeps the phantom (129 of 142 polls); builds are not byte-reproducible; the idle RSS of the first run of the rebuilt executable was 26.46 MB (runs 2-5 13.06-13.08 MB); not investigated.

## 2026-10-07 -- Full Observability, second follow-up: poisoned committer, sibling counter, D8 wording, build identity
Scope: observability only; protected paths untouched. Code commit `72dc7a2`; documentation commit above it (hash in the return report). Record: `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` section 26.
Implemented: ADR-OBS-01 step 3 -- `instance.coordinator_state` is `poisoned` when the committer's poison bit is set or the pool is `Failed`, `instance.healthy` is `failed` with it; `/readyz` and `instance.readiness` frozen at constant true (explicit exemption); ADR-OBS-01 PROPOSED -> ACCEPTED. `/v1/admin/status` `wal.sync_failures` is `null` (key kept; ADR-OBS-03 scope extended, no new ADR); CLI line keeps `sync_failures=` and prints `-` (existing null rendering), GUI shows `-`.
Measured / verified: in-process test with the fsync-fault hook -- `wal.poisoned`, `coordinator_state` and `healthy` flip together, `/readyz` stays true; with the bit hidden from the sampler the test fails. Real process (`72dc7a297b84`, 153 polls, 4 writers): `(alive, healthy, ready)` in every poll, `wal.sync_failures` null in 153 of 153 (before 129 of 142 non-zero). `cargo test -p rubixdb-api` all pass (observability 36/36), CLI `observability_integration` 6/6, fmt and clippy `-D warnings` clean, `tsc --noEmit` clean.
Classified: build non-reproducibility = linker timestamp (PE header + three debug-directory entries) and PDB GUID, 24-28 bytes per pair, code and data identical; two builds with release.ps1's `/Brepro` + remap flags gave identical SHA-256 (`1E7045E6...E078`), so identity is the `git_revision` string and hashes only identify files. Read p95 outlier = session variance within the combined baseline range; 120 s idle-CPU readings = short-window resolution effect (600 s windows 0.000 %).
D8 handle criterion final wording: retention delta <= the pre-observability binary's and absolute count within +/-2 of it under the same workload; 30-minute soak +10 vs +10, 122 vs 121 -> A83 PASS.
Matrix A: 85 rows -- PASS 69, FAIL 0, OPEN 1 (A66), NOT REQUIRED 6, NOT TESTED 9, NOT IMPLEMENTED 0. A80 OPEN -> PASS; A85 new. Observability not declared certified; whole product not declared production ready.

## 2026-10-07 -- Full Observability: final closure
Observability closure completed. Row A66 was reclassified by the maintainer from OPEN to NOT TESTED - ENVIRONMENT LIMITATION (not PASS, not FAIL; closes when the original host conditions can be reproduced or a root cause is found). Section A is 85 rows: 69 PASS + 0 FAIL + 0 OPEN + 6 NOT REQUIRED + 10 NOT TESTED + 0 NOT IMPLEMENTED = 85, and the observability implementation status is PASS on that basis (the NOT REQUIRED and NOT TESTED rows are enumerated, with reasons, in `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` section 27.3). The workspace regression status is FAIL, pre-existing only (the two `repo_hygiene` tests and the intermittent `m1_3_thousand_writers_throughput`; nothing introduced by the observability work remains). Whole-product production readiness is not declared: it is a separate certification that has not been performed.

## 2026-10-07 -- Full Observability: coverage-gap closure (A12, A14, A16)
Three NOT TESTED rows that were test-coverage gaps, not environment limits, are closed with tests only (no production file, protected path, dependency or `unsafe` touched; `sampler.rs` not in the diff): A12 (`errors.*`, two `limits.*` fields and `storage_state`: presence, type, idle values, `errors.wal_sync_failures` asserted as the value null, SQL error counter rises on a parse error, `http_server_errors_since_start` >= the three 5xx observed with the fsync-fault seam and never decreasing, `wal_write_errors` rises), A14 (the real router with the sampler never started: `stale: false`, `age_ms: null`), and A16 (new file `api/tests/observability_sampler_failure.rs`: five consecutive probe panics -> `degraded` then `failed` at `FAIL_AFTER_PANICS = 5`, the WARN logged once, recovery to `running` on the next good tick, `/healthz` and SQL answering throughout). Two value-trigger cases inside A12 stay NOT TESTED as sub-items of the PASS row (backpressure rejections: no API-level test exists; `sql_resource_limit_hits`: the 1,024-conjunct statement is refused by the front-end chained-operator limit before the planner counter). Section A is now 85 rows: 72 PASS + 0 FAIL + 0 OPEN + 6 NOT REQUIRED + 7 NOT TESTED + 0 NOT IMPLEMENTED = 85; certification section 28. Workspace regression, 2 h soak and 30-minute soak not run. Whole-product readiness not declared.

## 2026-10-07 -- Item C: Tokio blocking-pool growth / resource containment
Phase A (read-only, `PHASE_ITEM_C_DISCOVERY.md`): every SQL statement runs on tokio's blocking pool (`api/src/routes/sql.rs:211`), whose cap is the default 512 with a 10 s keep-alive (`cli/src/host.rs:180-183`); with connections opened inside the timed window the pool burst to 165-530 threads (530 = 18 + 512) and cost throughput and tail; capping it was the intervention that established the mechanism. Phase B (ADR-ITEM-C-01, `PHASE_ITEM_C_ADR.md`, commit `a5a9bbe`): new setting `RUBIXDB_LOCAL_MAX_BLOCKING_THREADS` (16..=512, default 512 = unchanged) in `cli/src/startup_env.rs` and `cli/src/host.rs`, three unit tests and a real-process test (`cli/tests/blocking_pool_cap_integration.rs`); no `api/` change, no protected path, no dependency. Phase C (`PHASE_ITEM_C_CERTIFICATION.md`): interleaved 3-round comparison of the pre-change binary `36db7b3` and the fixed binary at seven read and five write levels, plus a 4-round cold-start block and a 6-round re-check at 64 clients. Cap 16/64: threads exactly 18 + cap, cold-start 29.5-30.6k req/s vs 24.0-29.2k, p99 1.14-1.24 ms vs 1.32-2.53 ms, warm and write neutral. Setting unset: no regression demonstrated (a 64-client flag in the first comparison was not reproduced by the re-check; 2 of 9 fixed runs were low outliers, cause not established). Tests: lib 609/609 debug and release, sql 335, cli and api suites pass, fmt/clippy `-D warnings`/check clean; the full workspace regression was not re-run. The default was not changed (the data support 16-64; shipping a lower default is the maintainer's decision). Separate findings recorded in OPEN_ITEMS.md, not fixed: COMMIT runs on an async worker; intermittent `observability.rs` failures. Whole-product readiness not declared.
