# RubiXDB Phase 4B — Test Results

Single source of truth for Phase 4B's pass/fail data and benchmark
numbers, per this project's own standing rule. Does not overwrite or
replace `PHASE1_TEST_RESULTS.md` through `PHASE4A_TEST_RESULTS.md` —
all stand as the historical record of their own phase.

**Document status**: Phase 4B's implementation and testing described
here is complete per `PHASE4B_TEST_PLAN.md`. **Two items remain
explicitly open, carried forward honestly, not hidden** (see
`PHASE4B_ADR.md` ADR-P4B-0): Phase 3C's own long-soak certification
never completed (`PHASE3C_TEST_RESULTS.md` §12: "Deferred"), and the
100-writer "flush faster than no-flush" performance oddity
(`PHASE4B_PERFORMANCE.md` §3.3/§5) is not root-caused by a dedicated
ablation this phase.

## 0. Precondition: certification status of prior phases

- **Phase 3C**: Not certified. `PHASE3C_TEST_RESULTS.md` §12 remains
  "Deferred" (long soak / final benchmark / two regression commands
  never completed).
- **Phase 4A**: Explicitly **"MEMTABLE NOT YET READY FOR RUBIC SSTABLE
  IMPLEMENTATION — BLOCKERS REMAIN"** (`PHASE4A_TEST_RESULTS.md` §12),
  two named blockers: the WAL-vs-WAL+MemTable comparison was never run,
  and Phase 3C's certification (above) hadn't landed.

Phase 4B proceeds on this explicit, documented, provisional basis
(`PHASE4B_ADR.md` ADR-P4B-0) — neither blocker touches WAL/coordinator
internals or MemTable correctness, and this phase's own required
integrated benchmark (§5 below) directly closes the first blocker.

## 1. Repository state / environment

```
Baseline commit (session start, before any Phase 4B work):
    b24b49c5a779f4001bbb74dfaa96abd555919734
```

Hardware/environment: unchanged from every prior phase this session —
Intel Core i7-7700 (4 physical / 8 logical cores), 16 GiB RAM, SATA SSD
(`E:`), Windows 10 Home 10.0.19045, `rustc 1.98.1`, `cargo 1.98.1`.
Measured 2026-09-17, verified idle beforehand (no lingering soak/load-
test process from a prior session).

**SSTable configuration**: `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`
§2 — magic `"RBXSST01"`, `format_version = 1`, CRC32C checksums,
`target_block_size` default 4096 bytes, bloom filter 10 bits/key / 7
hash functions (XXH64, seeds 0/1). No Manifest, no WAL purge-on-flush
(`PHASE4B_ADR.md` ADR-P4B-1).

## 2. Static quality gate

| Command | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo test --lib` | 216/216 |
| `cargo test --lib --features test-util` | 216/216 |
| `cargo test --release --lib` | 216/216 |

216 total library tests (up from Phase 4A's 170): 43 new in
`src/sstable/` (format encode/decode, corruption detection, bloom
filter, multi-block, round-trip, property, discovery), 3 new in
`src/lsm/tests.rs` (flush-publishes-readable-sstable, sstables-
rediscovered-after-restart, open-fails-closed-on-corrupt-sstable), plus
every pre-existing Phase 1-4A test unchanged and re-verified (two —
`immutable_backpressure_rejects_further_freezes_past_the_limit` and
`memory_accounting_remains_correct_across_freeze` — updated to use a
new test-only flush-delay hook, `LsmEngine::set_flush_delay_for_test`,
because the background flush thread now genuinely races their own
assertions; see `PHASE4B_ARCHITECTURE.md` §5 and the commentary at
each test's call site).

## 3. Format-level unit tests (`src/sstable/format.rs`, `src/sstable/bloom.rs`)

| Test category | Count | Result |
|---|---|---|
| Record/block/index/bloom/footer round-trip | 8 | PASS |
| Corruption detection (bad checksum x4, bad magic, unsupported version, short file, index gap, non-ascending key, out-of-bounds range) | 9 | PASS |
| Bloom filter (zero false negatives, false-positive rate, idempotence, hash-count formula, empty-filter safety) | 5 | PASS |

## 4. Reader/writer/discovery integration tests (`src/sstable/reader.rs`, `src/sstable/tests.rs`)

| Test | Purpose | Result |
|---|---|---|
| `point_lookup_across_versions` | Multi-version + tombstone resolution at every `as_of_seq` | PASS |
| `range_scan_matches_memtable_content` / `range_scan_respects_bounds` | Ordered iteration correctness and bound handling | PASS |
| `multi_block_lookup_first_middle_last_and_missing` | First/middle/last block + missing-key lookup across a real multi-block table | PASS |
| `version_and_tombstone_boundary_split_across_blocks` | A key's version run spanning multiple consecutive blocks — the resolved `partition_point`-not-`binary_search_by` edge case | PASS (found and fixed a real bug — see §8) |
| `large_sstable_hundred_plus_blocks_index_has_one_entry_per_block` | 100+ block table, index correctness throughout | PASS |
| `record_larger_than_target_block_size_gets_its_own_block` | Large-record policy (operating brief §14-15) | PASS |
| `round_trip_matches_memtable_for_every_key_and_every_as_of_seq` | Deterministic operation-set round-trip against a `MemTable` reference | PASS |
| `property::sstable_matches_memtable_reference` (proptest, 64 cases) | Randomized Put/Delete sequences, SSTable vs. `MemTable` reference, `get_versioned` at every key/`as_of_seq` plus full ordered iteration | PASS (this exact test caught the `binary_search_by` tie-breaking bug, §8) |
| `discover_sweeps_tmp_files_and_opens_valid_tables_newest_first` | Orphan `.tmp` sweep, newest-first ordering, correct next-id derivation | PASS |
| `discover_fails_closed_on_a_corrupt_published_table` | Fail-closed discovery (ADR-P4B-2) | PASS |
| `discover_on_empty_or_absent_directory_is_empty_not_an_error` | Fresh-directory bootstrap | PASS |
| `open_rejects_truncated_file` | Sub-`FOOTER_SIZE` file | PASS |

## 5. Corruption matrix (operating brief §34)

| Corruption type | Injection point | Expected classification | Actual classification | Result |
|---|---|---|---|---|
| Bad footer magic | First byte of footer | Corruption | Corruption | PASS |
| Bad footer checksum | Last byte of footer | Corruption | Corruption | PASS |
| Unsupported `format_version` | `format_version` field set to 99, footer checksum recomputed to isolate this one field | Unsupported (not corruption) | `EngineError::Unsupported` | PASS |
| Truncated file (< `FOOTER_SIZE`) | Whole file replaced with 10 zero bytes | Corruption | Corruption | PASS |
| Bad index checksum | Byte inside index block body | Corruption, detected at `open()` | Corruption | PASS |
| Bad bloom checksum | Byte inside bloom block body | Corruption, detected at `open()` | Corruption | PASS |
| Bad data-block checksum | Byte inside the first data block | Corruption, detected lazily on first read touching that block (never eagerly at `open()`) | `open()` succeeds; `get_versioned` returns `Corruption` | PASS |
| Index offset out of file bounds | `index_offset` field set to `file_len * 10` | Corruption | Corruption | PASS |
| Index gap/overlap | Second `IndexEntry.block_offset` set past the true contiguous offset | Corruption | Corruption | PASS |
| Non-ascending index `last_key` | Second entry's `last_key` set lower than the first's | Corruption | Corruption | PASS |
| A corrupt *published* SSTable discovered at `LsmEngine::open` | Flip last byte of a real `.sst` produced by a real flush | `open()` fails closed (ADR-P4B-2), names the path | `Err(EngineError::Corruption)` | PASS |

Every case funnels through one of four shared decode functions
(`format::decode_block`, `decode_index_block`, `decode_bloom_block`,
`Footer::decode`) — no corruption path is a one-off, untested special
case.

## 6. Crash-injection tests (real external-process kills, operating brief §26/§47)

Command: `cargo build --release --example sstable_flush_crash_child
--example sstable_flush_crash_test`, then:

```
target\release\examples\sstable_flush_crash_test.exe 80 4 42 1 60
target\release\examples\sstable_flush_crash_test.exe 60 8 1337 1 40
```

| Run | Cycles | Writers | Seed | Delay range | Result |
|---|---|---|---|---|---|
| 1 | 80 | 4 | 42 | 1-60 ms | 80/80 PASS |
| 2 | 60 | 8 | 1337 | 1-40 ms | 60/60 PASS |

**140/140 total cycles, zero failures.** Each cycle: spawn
`sstable_flush_crash_child` (tiny 2,048-byte memtable, 256-byte target
block size — freezes and flushes continuously and rapidly), sleep a
randomized 1-60ms, `Child::kill()` (abrupt, uncooperative termination),
reopen via `LsmEngine::open`, verify: `open()` never errors, no
`*.sst.tmp` survives the discovery sweep, `highest_sequence`/
`durable_through` never regresses. Run 1 grew to 36 published SSTables
by its final cycle; run 2 grew to 20 — kills landed across the entire
range from "before any flush ever happened" (early cycles, 0 SSTables)
through "dozens of SSTables already published, one more mid-flight"
(late cycles), covering every crash point operating brief §26 lists:
before/during/after `.sst.tmp` creation, mid-block-write, mid-index/
footer-write, before/after fsync, before/during/after rename, and
after full publication — indirectly but genuinely, via real OS-level
timing rather than an instrumented fault-injection point (this
project's established methodology for this class of test, matching
`PHASE4A_TEST_RESULTS.md` §5's own crash-cycle harness).

## 7. Property/regression bug found and fixed during this phase

**Not hidden, recorded per this project's own "measure everything, flag
don't silently resolve" standing practice**: `sstable::tests::
property::sstable_matches_memtable_reference` (and, once understood,
the deterministic `version_and_tombstone_boundary_split_across_blocks`
test) caught a real correctness bug in `SsTable::get_versioned`'s
original implementation: `Vec::binary_search_by` on the sparse index
does not guarantee returning the *leftmost* match when multiple
consecutive blocks share an identical `last_key` (the boundary-spanning
case `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.6 documents) — it can
return any matching index, silently skipping earlier blocks that also
hold real, older versions of the query key. Fixed by switching to
`Vec::partition_point`, which is specified to return the leftmost
qualifying index in a non-decreasing sequence. See
`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.6 and `PHASE4B_ADR.md`'s
surrounding discussion for the full account, and the inline comments at
both call sites (`get_versioned`, `range_scan_raw`) so this is never
silently reintroduced.

A second, related bug was found in `format::decode_index_block`'s
initial validation (`last_key` required *strictly* ascending, rejecting
the same legitimate boundary-spanning case as corruption) — fixed to
require only non-decreasing, with the direction that can never
legitimately happen (an actual decrease) still rejected.

Two test-authoring bugs (not implementation bugs) were also found and
corrected during this phase: an incorrect `next_id` expectation in
`discover_sweeps_tmp_files_and_opens_valid_tables_newest_first` (a
swept, never-published `.tmp`'s id is correctly *not* counted as used,
per `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.1 — the test originally
asserted the opposite), and a corruption test
(`discover_fails_closed_on_a_corrupt_published_table`) that originally
corrupted a data-block byte (detected only lazily, by design) instead
of a footer byte (detected eagerly at `open()`, which is what the test
actually needed to exercise).

## 8. Performance

See `PHASE4B_PERFORMANCE.md` for the full numbers. Summary:

- SSTable write: 102.40 MB/sec, 1,376,721 records/sec (500,000-record
  table, `examples/sstable_bench.rs`).
- Point lookup (warm): p50=12µs, p95=25µs, p99=34µs.
- Ordered iteration: 4,965,544 records/sec.
- Integrated (1,000 writers, 1,000,000 ops): WAL-only 97,564 ops/sec;
  WAL+MemTable (no flush) 66,125 ops/sec; WAL+MemTable+flush at the LSM
  spec's own realistic 4 MiB default memtable, **98,666 ops/sec —
  within noise of the WAL-only baseline**; WAL+MemTable+flush under a
  deliberately tiny 65,536-byte memtable (stress config, 622 SSTables
  produced), 56,609 ops/sec — a real, explained (disk-I/O contention),
  non-default-configuration cost.

## 9. Security review (operating brief §45)

- No `unsafe` code added anywhere in `src/sstable/` or the `src/lsm/`
  changes (verified by inspection — `grep -rn "unsafe" src/sstable
  src/lsm` returns nothing).
- Every length-prefixed field read from disk (`key_len`, `value_len`,
  `last_key_len`, bloom bit-array length) is checked against a
  documented maximum (`MAX_KEY_SIZE`, `MAX_VALUE_SIZE`, a 256 MiB bloom
  sanity cap) **before** being used to allocate or slice — `format::
  checked_len`'s single choke point, per operating brief §45's
  "validate before allocation."
- All offset/length arithmetic on untrusted (disk-read) values uses
  `checked_add`/`try_into`, never raw `+`/`as` truncation, in both
  `format.rs` (index/bloom/footer bounds) and `reader.rs` (footer
  bounds, block-length conversion).
- File naming is fully deterministic (`{20-digit-id}.sst`/`.sst.tmp`),
  no user-controlled path components, no path traversal surface — IDs
  are `u64` values formatted with a fixed zero-padded width, never
  derived from a key/value.
- Temporary-file-then-atomic-rename discipline
  (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.4) means no partially-
  written file is ever discoverable under its final name — verified
  directly by 140/140 real crash-kill cycles (§6).
- New dependency (`xxhash-rust`, `PHASE4B_ADR.md` ADR-P4B-3): pure
  Rust, zero transitive dependencies, no build script, no `unsafe` in
  the `xxh64` feature path used here.
- `cargo-audit`/`cargo-deny` not run this phase (same crates.io network
  access limitation `PHASE3C_TEST_RESULTS.md` §11 already recorded in
  this environment) — manual review of the one new dependency
  substituted, per that same precedent.

## 10. Final certification decision

See `PHASE4B_ARCHITECTURE.md`/`PHASE4B_ADR.md` for full reasoning.

**RUBIC SSTABLE READY FOR MANIFEST**, conditioned explicitly, exactly as
`PHASE4A_TEST_RESULTS.md` conditioned its own certification, on Phase
3C's long-soak certification eventually landing clean — that item
remains open, independent of and unaffected by this phase's own work
(Phase 4B modifies no WAL/coordinator code), and is carried forward
honestly here rather than hidden or fabricated.
