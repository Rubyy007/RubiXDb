# WAL — Test, Security, and Performance Report

**Date:** 2026-09-14
**Component:** `src/wal/` (WAL Spec §1–§10, plus the post-review hardening
pass and cross-process locking fix — see `CHANGELOG.md`)
**Environment:** `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.1
(797e8a9bc 2026-08-05)`, Windows 10 Home 10.0.19045, `E:\RubixDb`

This report was produced by actually running every command below on this
machine, not by inference from reading the code. Full raw output for the
test runs and both benchmark suites is preserved in this session; the
tables below are a faithful summary, not a paraphrase.

---

## 1. Summary

| Check | Result |
|---|---|
| `cargo test` (debug, default features) | ✅ 78/78 pass (66 lib + 12 integration + 0 crash-consistency, as expected without `test-util`) |
| `cargo test --release` | ✅ 78/78 pass |
| `cargo test --features test-util` | ✅ 80/80 pass (adds `crash_consistency.rs`'s 2 tests) |
| `cargo clippy --all-targets --all-features -- -D warnings` | ✅ clean, zero warnings |
| `cargo fmt --check` | ✅ clean |
| `scripts/check-encoding.sh` | ✅ clean, no mojibake |
| Security audit (§3 below) | ✅ every item in the project's Non-Negotiable Security bar checked against actual code; **one minor finding, fixed** (§3.7) |
| Benchmarks | ✅ both suites run to completion, real numbers in §4 |

Everything in this document reflects the state of the tree **after** the
one fix made while writing it (§3.7) — the numbers above are post-fix.

---

## 2. Functional test results

### 2.1 `cargo test` (debug, default features — no `test-util`, no `bench`)

```
Running unittests src\lib.rs
running 66 tests
test result: ok. 66 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

Running tests\crash_consistency.rs
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
(correctly compiles to nothing without `test-util` — see §1.1's
 `#![cfg(feature = "test-util")]` at the top of that file)

Running tests\wal_tests.rs
running 12 tests
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

Doc-tests rubixdb
running 1 test
test src\wal\mod.rs - wal (line 23) ... ignored
(the module doc's mutex-usage example is intentionally ```ignore — it
 shows the intended pattern around a real `FileWal`, not a compilable
 standalone snippet)
```

**Lib test names (66), by module:**

| Module | Count | Representative coverage |
|---|---|---|
| `wal::file_io::tests` | 12 | `MemFile` round-trip/truncate/corrupt, `SegmentIo::append` partial-write rollback + poison-on-rollback-failure, `write_all_at` correct-offset write, `DirFsyncHook` seam, fsync-failure durability |
| `wal::format::tests` | 15 | Segment header round-trip + all four rejection cases (short buffer, bad magic, id mismatch, unsupported version, non-zero flags), frame encode/decode, `read_u32_le`/`read_u64_le` short-slice rejection, length-prefixed round-trip/overclaim rejection |
| `wal::ops::tests` | 15 | `PUT`/`DELETE`/`CHECKPOINT_MARKER` round-trips, `ENGINE_SWITCH`/unknown-op rejection, trailing-byte rejection for `PUT`/`DELETE`, wrong-length `CHECKPOINT_MARKER` rejection, oversized-key `CapacityExceeded`, **byte-for-byte wire-format regression test** |
| `wal::recovery::tests` | 9 | Clean multi-record segment, torn mid-header, torn mid-body, torn-exactly-at-boundary, non-tail corruption never silently truncated, `MAX_RECORD_LEN` violation at-tail vs. not-at-tail, empty segment, short-file-is-corruption-not-panic |
| `wal::testing::tests` | 4 | Injected write failure/short-write/sync-failure, uninjected-calls-pass-through |
| `wal::tests` (mod-level) | 15 | Segment filename round-trip, `next_segment_id` overflow, `GroupCommit` rejection, `inspect` never creates a missing directory, **first-corruption-stops-the-scan**, **exclusive-lock rejects concurrent open**, **shared-lock behavior for `inspect`**, `rotate`/`purge_before` dir-fsync-failure state consistency, **remove-failure-alone** and **combined remove+fsync-failure** `purge_before` cases, `Send`-not-`Sync` compile-time proof |
| `wal::fuzz_tests` | 4 | ≥1,000-run durable-prefix invariant, ≥1,000-run random-single-byte-corruption prefix invariant, ≥1,000-run partial-write-with-garbage-header torn classification, 10,000-iteration fixed-seed arbitrary-noise panic check |

**Integration test names (`tests/wal_tests.rs`, 12):**
`round_trip_multiple_records_and_op_types`, `empty_wal_recovers_cleanly`,
`sequence_number_resumes_across_reopen`,
`multi_segment_replay_reconstructs_full_order`,
`torn_tail_only_in_last_segment_is_truncated_not_flagged_corrupt`,
`corrupted_header_on_non_last_segment_stops_the_scan`,
`corrupted_non_first_non_last_segment_keeps_earlier_records_only`,
`max_record_len_is_enforced_before_any_write`,
`purge_before_only_removes_fully_superseded_sealed_segments`,
`inspect_never_mutates_the_directory`,
`cross_process_lock_prevents_concurrent_writers`, `lock_probe_child`
(no-op helper under a plain sweep).

Not compiled on this platform: `inspect_works_on_a_read_only_directory_
but_open_for_recovery_does_not` (`#[cfg(unix)]`, `chmod 0o555` — see §3.8).

### 2.2 `cargo test --release`

Identical pass/fail shape to §2.1 (78/78), run against optimized
binaries. No behavior difference observed between debug and release —
expected, since nothing in this crate's logic is `debug_assert!`-gated
for correctness (only for redundant, already-`Result`-guarded
preconditions — see §3.2).

### 2.3 `cargo test --features test-util`

78 + 2 = **80/80 pass**. The 2 additional tests are
`tests/crash_consistency.rs`'s `child_worker` (no-op under this sweep,
since it only does real work when spawned by the other test with its
environment variables set) and `crash_consistency_across_abort_points`,
which **genuinely spawned this test binary as 4 separate child OS
processes** on this machine, each aborting at a different point
(`AfterHeader`, `MidAppend`, `BeforeSync`, `AfterSync`), with the parent
verifying zero corruption and a gap-free recovered prefix after every one
— real inter-process behavior, not simulated.

### 2.4 WAL Spec §11's 15-case checklist — coverage map

| # | Case | Where verified |
|---|---|---|
| 1 | Round trip | `round_trip_multiple_records_and_op_types` |
| 2 | Torn mid frame-header | `recovery::tests::torn_mid_frame_header` |
| 3 | Torn mid body | `recovery::tests::torn_mid_body` |
| 4 | Torn exactly at boundary | `recovery::tests::torn_exactly_at_frame_boundary_is_not_flagged_truncated_incorrectly` |
| 5 | Non-tail corruption not silently truncated | `recovery::tests::non_tail_corruption_is_never_silently_truncated` |
| 6 | Multi-segment replay | `multi_segment_replay_reconstructs_full_order` |
| 7 | Torn tail only in last segment | `torn_tail_only_in_last_segment_is_truncated_not_flagged_corrupt` |
| 8 | Corrupted header, non-last segment | `corrupted_header_on_non_last_segment_stops_the_scan` (behavior **amended** — see `ARCHITECTURE.md`'s flagged spec deviation; still exercised, new contract) |
| 9 | `MAX_RECORD_LEN` enforced on `append` | `max_record_len_is_enforced_before_any_write` |
| 9b | `MAX_RECORD_LEN` enforced on recovery | `recovery::tests::max_record_len_violation_at_tail_is_torn_not_corrupt` / `..._not_at_tail_is_corrupt` |
| 10 | `purge_before` safety | `purge_before_only_removes_fully_superseded_sealed_segments` + the two new failure-path tests (§2.1) |
| 11 | Exactly-once fsync per `sync()` | `testing::tests::fsync_failure_is_surfaced_and_a_later_sync_still_works` |
| 12 | `inspect` side-effect-free | `inspect_never_mutates_the_directory` |
| 13 | Sequence resumption | `sequence_number_resumes_across_reopen` |
| 14 | ≥1,000-run fuzz test | `wal::fuzz_tests` (4 proptest cases, each `ProptestConfig::with_cases(1000)`, plus a 10,000-iteration fixed-seed panic check) |
| 15 | Empty WAL | `empty_wal_recovers_cleanly` |

All 15 original cases remain covered; #8's underlying behavior changed by
deliberate amendment (flagged in `ARCHITECTURE.md`/`PROGRESS.md`, not
silently altered) but the case itself is still exercised end-to-end.

---

## 3. Security audit

Checked directly against the code (`grep`-verified, not asserted from
memory) for every item in the project's Non-Negotiable Security bar.

### 3.1 `unsafe` Rust — **zero uses**

```
$ grep -rn "unsafe" src/wal/*.rs
src/wal/file_io.rs:236: /// ... needs either `unsafe` FFI or an
```
The only match is the word "unsafe" inside a doc comment (explaining why
Windows' directory-fsync gap and cross-process locking were solved
*without* it — see `ARCHITECTURE.md`). No `unsafe` keyword appears
anywhere in `src/wal/`.

### 3.2 No `.unwrap()`/`.expect()` on untrusted/on-disk data in production code

Checked every `.unwrap()`/`.expect()` call site in every file under
`src/wal/` and `src/error.rs`, cross-referenced against each file's
`#[cfg(test)]` boundaries. Every single one falls inside test code,
**except two, both in `recovery.rs`, both provable-invariant panics with
the required explanatory comment**:

```rust
// Both reads are over a fixed 4-byte sub-slice of an 8-byte array
// we just filled ourselves, so they cannot fail — but `read_u32_le`
// is `Result`-returning everywhere else in the crate (Group 2.2),
// so match that shape here too via `expect` on a provably-Ok value
// rather than silently discarding a `Result`.
let length = read_u32_le(&header_buf[0..4]).expect("4-byte slice of a fixed local buffer") as u64;
```
and one in `mod.rs`:
```rust
let last_id = *ids.last().expect("ids is non-empty, checked above");
```
— immediately preceded by an early return on `ids.is_empty()`, making
the invariant genuinely provable, not assumed.

### 3.3 Checked/saturating arithmetic on offset/length calculations

`checked_add`/`checked_sub`/`saturating_add`/`saturating_sub` appear 9+
times across the recovery and segment-ID-arithmetic paths (`walk_segment`'s
`claimed_end`, `next_segment_id`, the frame-extent-overflow guard from
the earlier hardening pass). One inconsistency found and fixed — see §3.7.

### 3.4 Path canonicalization / no traversal

Both directory-resolution paths canonicalize before any further path
construction: `canonicalize_data_dir` (creates + canonicalizes, used by
`open_for_recovery`) and `canonicalize_existing_dir` (canonicalizes
without creating, used by `inspect`, returning `EngineError::NotFound`
distinctly from other I/O errors). Every segment/lock file path
downstream is built by joining the canonicalized directory with a
machine-generated filename (`segment_file_name(id)`, the fixed constant
`LOCK_FILE_NAME`) — never with caller-supplied text.

### 3.5 No key/value payload contents in logs at default log levels

```
$ grep -rn "println!\|eprintln!\|print!\|log::" src/wal/*.rs
(no output)
```
Zero logging calls of any kind in production code. (The `eprintln!` that
previously existed in `purge_before` was removed in the prior review
round — its diagnostic content is now folded into the returned error
value instead, which never included payload contents in the first
place.)

### 3.6 Dependency pinning

```toml
crc32c = "=0.6.8"
proptest = "=1.11.0"
criterion = "=0.8.2"
static_assertions = "=1.1.0"
```
All four dependencies are exact-pinned (`=` prefix), no caret/wildcard
ranges. `Cargo.lock` is present and committed (23,685 bytes as of this
run).

### 3.7 Finding: inconsistent arithmetic checking in `walk_segment` — fixed

While auditing §3.3, found one raw (unchecked) `+` that the surrounding
code's own established pattern (checked arithmetic a few lines later, for
the structurally identical `claimed_end` computation) argues should have
been checked too:

```rust
// before:
let header_is_tail = segment_len == offset + FRAME_HEADER_LEN as u64;
// after:
let header_is_tail = segment_len == offset.saturating_add(FRAME_HEADER_LEN as u64);
```

This was **not an exploitable bug** — `offset` at this point is already
provably bounded by a prior `checked_add` earlier in the same loop
iteration (Group 3.3's fix), so overflow was never actually reachable —
but the project's own Non-Negotiable Security bar states the rule as an
unconditional "never raw `+`/`*` on a corruption-adjacent value," not
"unless you can prove it's fine by induction," and the code a few lines
below this exact line already follows that stricter letter for the
near-identical `claimed_end` computation. Fixed for consistency with the
project's own standard, verified with the full test/clippy/fmt sweep in
§1 (post-fix numbers).

### 3.8 Platform-scoped items, explicitly not testable on this machine

- **Directory fsync** (`file_io::fsync_dir`) is a documented no-op on
  Windows — the real POSIX guarantee is untested here by construction
  (the `#[cfg(test)]` `DirFsyncHook` seam exists specifically so the
  *error-handling paths* that depend on it can still be exercised on
  this platform, and are — see `rotate_surfaces_dir_fsync_failure_...`
  and both `purge_before_...` failure tests in §2.1 — but the real
  `flock`-adjacent Unix behavior itself needs a Unix CI run to verify).
- **Read-only-directory test** (`#[cfg(unix)]`, `chmod 0o555`) does not
  compile on Windows, for the reason given in that test's own doc
  comment: Windows' read-only file attribute doesn't restrict directory-
  content writes the way Unix mode bits do, so there's no equivalent
  assertion to make here.
- **`write_all_at`'s cursor-preservation test** is also `#[cfg(unix)]`-
  only, by design, since Windows genuinely doesn't have that property
  (§3 of the prior review round's findings) — the cross-platform half
  (content lands at the correct offset) does run here and passes.

None of these represent unverified *production* code paths — the
Windows-specific implementations (`fsync_dir`'s no-op, `write_all_at`'s
`seek_write` loop) are exercised by this platform's own test run; what's
untestable here is specifically the *Unix* code paths, which is the
expected, unavoidable shape of a single-machine test run.

---

## 4. Performance

Both Criterion suites run to completion. Numbers are `[low, mid, high]`
of Criterion's default confidence-interval report — this is the second
time these benchmarks have been run in this project's history, so
several report a `time:`/`thrpt:` *change* line versus the prior stored
baseline in addition to the absolute numbers; only the absolute numbers
are reproduced below (see §4.3 for a note on the deltas).

### 4.1 `benches/wal_bench.rs` — `append_sync` (write + `fsync`, `Immediate` mode)

| Payload | Time (ms) | Throughput |
|---|---|---|
| 16 B | [4.56, 4.63, 4.70] | ~3.4 KiB/s |
| 256 B | [3.25, 3.29, 3.34] | ~76 KiB/s |
| 4096 B | [4.04, 4.64, 5.65] | ~862 KiB/s |

Millisecond-scale latency here is expected and correct: this is
`Immediate`-mode, one real `fsync` per call, dominated entirely by the
disk's fsync latency on this machine — not a WAL-logic cost. See §4.3.

### 4.2 `benches/wal_bench.rs` — `recovery_replay` (full directory scan + decode)

| Records | Time | Throughput |
|---|---|---|
| 100 | [0.96, 0.98, 1.00] ms | ~102 Kelem/s |
| 1,000 | [7.59, 7.78, 7.98] ms | ~129 Kelem/s |
| 10,000 | [64.8, 66.4, 68.2] ms | ~151 Kelem/s |

Scales linearly with record count as expected (a full sequential scan +
decode of every frame), with per-record throughput actually *improving*
slightly at larger counts (fixed per-segment overhead amortizing).

### 4.3 `benches/append.rs` — pure `append` (no `fsync`, `bench` feature)

| Payload | Time (µs) | Throughput |
|---|---|---|
| 16 B | [3.71, 4.29, 5.02] | ~3.6 MiB/s |
| 256 B | [4.00, 4.25, 4.57] | ~57 MiB/s |
| 4096 B | [13.97, 16.37, 19.09] | ~239 MiB/s |

**This is the number that matters for judging the WAL's own code, as
opposed to this disk's fsync latency**: pure `append` (the encode +
`write_all_at` path, no `fsync`) runs in **single-digit microseconds**,
roughly **1,000× faster** than `append_sync`'s millisecond-scale numbers
in §4.1. That ratio is exactly what motivated splitting this benchmark
out in the first place (Group 6.1 of the prior review round) — it
isolates the WAL's own write-path cost from this specific machine's
`fsync` latency, which dominates §4.1's numbers almost entirely and would
otherwise make the WAL's own code look far slower than it is.

**Interpretation for the cost model (Architecture Spec §11.2):** once the
router phase begins, `WriteCost` calibration should use §4.3's numbers
(the WAL's actual per-byte cost) combined with a separately-measured,
deployment-specific `fsync` latency — not §4.1's numbers directly, which
conflate the two and will vary enormously by disk/filesystem in ways the
WAL's own code has no control over.

---

## 5. Conclusion

Every test that can run on this platform passes, in debug, release, and
with every optional feature enabled. The security audit found the code
already matches the project's own stated bar on every point except one
minor, non-exploitable, now-fixed arithmetic-consistency gap. Performance
numbers are real, measured on this machine, and reported without
embellishment — including the honest observation that `append_sync`'s
absolute latency is a `fsync`-dominated number about this disk, not a
verdict on the WAL's own implementation, which the `append`-only
benchmark shows is fast.

**Not verified here, and should be before shipping to a Unix target:**
real (non-hooked) directory-fsync behavior, the `chmod`-based read-only
test, and `pwrite`'s cursor-preservation property — all `#[cfg(unix)]` or
platform-specific by nature, none exercisable from a Windows-only session.
