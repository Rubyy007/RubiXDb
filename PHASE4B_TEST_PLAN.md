# RubiXDB Phase 4B — Test Plan

## 1. Scope

Test the RUBIC SSTable component (`src/sstable/`) and its flush
integration into `LsmEngine` (`src/lsm/mod.rs`), per
`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` and `PHASE4B_ARCHITECTURE.md`.
Explicitly **not** in scope: Manifest, WAL purge-on-flush, or
compaction (`PHASE4B_ADR.md` ADR-P4B-1, operating brief §52-§53) — no
test in this plan exercises any of those, since none exist this phase.

## 2. Unit / format-level tests (`src/sstable/format.rs`, `src/sstable/bloom.rs`)

- Record, block, index-block, bloom-block, and footer encode/decode
  round-trips.
- Corruption detection at each structural boundary: bad block checksum
  (body and trailer separately), bad index/bloom/footer checksum, bad
  footer magic, unsupported `format_version`, truncated file, index
  gap/overlap, non-ascending `last_key`, out-of-bounds block range.
- Bloom filter: zero false negatives (exhaustive over every inserted
  key), false-positive rate within a reasonable bound of the ~1% target
  at 10 bits/key (statistical), repeated-insertion idempotence, the
  spec's exact hash-function-count formula.

## 3. Reader/writer integration tests (`src/sstable/reader.rs`, `src/sstable/tests.rs`)

- Point lookup across multiple versions and a tombstone, at every
  `as_of_seq` boundary.
- Multi-block: first/middle/last-block lookup, missing keys, a key
  spanning a block boundary (multiple versions split across two or more
  consecutive blocks — the resolved edge case documented in
  `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.6).
- Large-record policy: a record larger than `target_block_size` gets
  its own block, never splits, never zero-record blocks.
- 100+ block SSTable: index has exactly one entry per block, lookups
  remain correct throughout.
- Round-trip against a `MemTable` reference at every `(key, as_of_seq)`
  pair for a deterministic operation sequence.
- Property test (`proptest`, 64 cases, randomized Put/Delete
  sequences): SSTable-via-`get_versioned`/`range_scan_raw` matches a
  direct `MemTable` reference exactly, including tombstones and
  multi-version resolution.
- Discovery (`sstable::discover`): sweeps orphaned `.sst.tmp` files
  unconditionally, opens valid `.sst` files newest-first, computes the
  correct next id from existing filenames (never counting a swept,
  never-published `.tmp`'s id as "used"), fails closed (returns `Err`,
  names the path) on a corrupt published table, treats an absent/empty
  directory as "nothing to discover," not an error.

## 4. LSM flush integration tests (`src/lsm/tests.rs`)

- A flush moves data into a published, independently-correct SSTable
  that remains readable via `LsmEngine::get`/`get_as_of` regardless of
  which tier (active/immutable/sstable) currently holds it.
- SSTables are rediscovered after a restart, and reads remain correct
  from either source (WAL replay into a fresh MemTable, or the
  rediscovered SSTables) — deliberately redundant, per ADR-P4B-1.
- `LsmEngine::open` fails closed when a previously-published SSTable is
  corrupt.
- Every pre-existing Phase 4A test continues to pass, updated only
  where the flush thread's real background activity legitimately
  changes observable timing (`immutable_backpressure_rejects_further_
  freezes_past_the_limit`, `memory_accounting_remains_correct_across_
  freeze` — both made deterministic via a new test-only flush-delay
  hook rather than left racy).

## 5. Crash-injection tests (`examples/sstable_flush_crash_test.rs` + `_child.rs`)

Real external-process kills (`Child::kill()`), not self-inflicted
`abort()`, at a randomized-but-seeded short delay (1-60ms), against a
child configured with a tiny memtable/block size so kills land
throughout the flush pipeline across many cycles. After each kill, the
parent reopens via `LsmEngine::open` and verifies: `open()` never
errors, no `*.sst.tmp` survives the sweep, the `highest_sequence`/
`durable_through` watermark never regresses. Two independent seeds,
≥60 cycles each.

## 6. Performance (`examples/sstable_bench.rs`, `examples/lsm_flush_load_test.rs`)

Measured separately per operating brief §39: SSTable write throughput
(MB/sec, records/sec), point-lookup p50/p95/p99, ordered iteration
throughput, bloom-filter effectiveness, writer/reader memory. Then the
integrated three-way comparison (operating brief §40-41): WAL-only vs.
WAL+MemTable vs. WAL+MemTable+SSTable-flush, at 100 and 1,000 writers,
under both a stress (tiny memtable) and realistic (spec-default 4 MiB
memtable) configuration.

## 7. Static quality gate

`cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --lib`, `cargo test --release --lib`, `cargo
test --lib --features test-util` — all required clean before
certification, per operating brief §48.

## 8. Explicitly out of scope this phase

- Manifest-based liveness/checkpoint tests (no Manifest exists).
- WAL-purge-after-flush tests (no purge call exists, by design).
- Compaction tests (no compaction exists).
- An exhaustive, field-by-field corruption matrix covering every single
  byte offset in every structure (the corruption tests in §2-§3 cover
  one representative case per structural boundary/check, not every
  possible bit flip) — judged sufficient given every check funnels
  through a small number of shared decode functions
  (`format::decode_block`, `decode_index_block`, `decode_bloom_block`,
  `Footer::decode`), each exercised by at least one corruption test.
