# RUBIC SSTable Format Specification (Phase 4B)

Status: **Final for Phase 4B — ready for implementation.**

Governance: subordinate to `RUBIC_FORMAT_SPECIFICATION.md` (family
policy) and implements, byte-for-byte, `RubixDB-LSM-Engine-
Specification-v1.0.md` §2-§3 ("Status: Final — ready for
implementation"). Per the specification hierarchy (operating brief §5):
where this document states a concrete byte layout, it is quoting or
directly deriving from that already-final spec, never re-deciding it.
Where this document resolves something that spec left to a future
Manifest/compaction phase, that resolution is marked **PHASE-4B
DECISION** and justified explicitly — never silently inherited or
guessed.

---

## 1. Scope of this document

`RubixDB-LSM-Engine-Specification-v1.0.md` §2-§3 already fully
specifies the SSTable byte layout. This document does three things
that spec does not:

1. Consolidates that byte layout into one place for implementers,
   quoting it exactly (Section 2 below).
2. Records the **Phase-4B-local decisions** required to build and read
   SSTables *without* the Manifest — a component explicitly out of
   scope this phase (operating brief §52) — while remaining fully
   forward-compatible with a future Manifest that adopts the same file
   set (Section 3).
3. Records Windows-specific durability behavior this project's Windows
   development/test environment requires documenting (operating brief
   §9), reusing the WAL's own already-established, already-tested
   platform primitive rather than inventing a second one (Section 4).

---

## 2. Byte layout (quoted from `RubixDB-LSM-Engine-Specification-v1.0.md`, unchanged)

### 2.1 File magic, format version, endianness, integer widths

- Magic (footer): `"RBXSST01"`, 8 bytes, ASCII.
- `format_version`: `u32` LE = `1`. Current version: `1`. Supported
  versions on read: `{1}`. Any other value: fail closed
  (`RUBIC_FORMAT_SPECIFICATION.md` §3.3) —
  `EngineError::Unsupported { operation }` naming the found version,
  never a best-effort partial read. **No separate `SstableError` type
  is introduced** — per `src/error.rs`'s own doc comment, every Phase
  0 component (`wal`, `manifest`, `sstable`, `lsm`, `compaction`)
  returns `Result<T>` over the one shared `EngineError` enum; `sstable`
  follows this exactly, using `Corruption { detail }` for structural/
  checksum failures, `Unsupported { operation }` for version
  rejection, and `CapacityExceeded { requested, max }` for
  oversized-field rejection (Section 2.9), each `detail`/`operation`
  string naming which structure and check failed (never the payload
  bytes themselves, per this project's standing security rule).
- Endianness: little-endian, every multi-byte integer field, no
  exception (`RUBIC_FORMAT_SPECIFICATION.md` §3.4).
- Integer widths: fixed-width only (`u8`, `u32` LE, `u64` LE) — no
  varint/LEB128 anywhere (`RUBIC_FORMAT_SPECIFICATION.md` §3.5).
- Checksum: CRC32C (Castagnoli), the `crc32c` crate already a
  dependency of this project (`Cargo.toml`) — no new checksum
  dependency.

### 2.2 File layout

```
+----------------------------------+
| Data Block 0                     |
| Data Block 1                     |
| ...                              |
| Data Block N-1                   |
+----------------------------------+
| Bloom Filter Block                |
+----------------------------------+
| Index Block                       |
+----------------------------------+
| Footer (fixed 72 bytes)           |
+----------------------------------+
```

The footer is fixed-size and sits at the file's end: `file_len - 72`
locates it. A reader validates the footer first, before trusting any
other offset in the file (operating brief §10, §19 — never parse
arbitrary offsets before validating file boundaries).

### 2.3 Data record

```
Record :=
  key_len   : u32 LE
  key       : [u8; key_len]
  seq       : u64 LE
  op        : u8         // 1 = PUT, 2 = DELETE
  value_len : u32 LE     // 0 when op == DELETE
  value     : [u8; value_len]
```

Records within a data block are sorted `(key ascending, seq
ascending)` — identical to the MemTable's own ordering
(`RubixDB-LSM-Engine-Specification-v1.0.md` §1.1); no re-sort step
exists anywhere in the write path (operating brief §12).

### 2.4 Data block

```
Block :=
  record_count : u32 LE
  records      : Record*     // record_count of them, concatenated
  checksum     : u32 LE      // CRC32C over (record_count bytes || all record bytes)
```

`target_block_size` default: **4096 bytes** (config, `LsmConfig::
sstable_target_block_size`). Soft threshold: a record is never split
across a block boundary; the last record of a block may push it over
target. Prefix compression / restart points: **not implemented**
(explicit v1 scope cut, matches the LSM spec exactly).

**Large-record policy (operating brief §15, resolved, not guessed):**
if a single record's encoded size exceeds `target_block_size`, it is
still placed whole into its own block (a block containing exactly one
oversized record is valid — "soft threshold" already implies this: the
spec's own wording, "the last record of a block may push it slightly
over the target," is the general case of which "a single record alone
exceeds the target" is the extreme case, not a different rule). The
writer never splits a record, never infinite-loops, never emits a
zero-record block, and never silently drops or truncates a
too-large record. The **only** hard ceiling is `u32::MAX` for
`key_len`/`value_len` individually (the field width itself) and a
configured `max_key_size`/`max_value_size` (Section 2.9) enforced
*before* allocation, both on write and on read.

### 2.5 Bloom filter block

```
Bloom Filter Block :=
  num_bits            : u64 LE
  num_hash_functions  : u8
  bits                : [u8; ceil(num_bits / 8)]
  checksum            : u32 LE   // CRC32C over everything above
```

One filter per SSTable, covering every key in the file. Parameters,
pinned for v1 (per spec): **10 bits per key**,
`num_hash_functions = round(10 * ln(2)) = 7`, expected false-positive
rate ~1%. Hashing: two independent 64-bit XXH64 hashes, seeds `0` and
`1`, combined by double hashing (Kirsch-Mitzenmacher):
`bit_i(key) = (h1(key) + i * h2(key)) mod num_bits` for `i` in
`0..num_hash_functions`. A bloom filter never produces a false
negative; a false positive only ever costs one wasted block read
(operating brief §17) — it is never the final correctness authority
(Section 2.7's `get_versioned` algorithm always falls through to the
real block scan on a "maybe present" answer).

**Crate choice (PHASE-4B DECISION, dependency policy §46):**
`xxhash-rust`, pinned exact version, `xxh64` feature only. Rationale:
the algorithm (XXH64) is mandated by the already-final LSM spec, not
chosen here — only the crate implementing it is a Phase-4B decision.
`xxhash-rust` is pure-Rust (no `unsafe`, no C build dependency, no
build-script requirement — relevant on this project's Windows
toolchain), has no transitive dependencies of its own, and exposes the
exact seeded-64-bit-hash primitive the spec's formula needs
(`xxh64::xxh64(bytes, seed) -> u64`) with no unnecessary API surface.
No license concerns (BSL/MIT dual, permissive). See `PHASE4B_ADR.md`
for the full dependency-policy write-up.

### 2.6 Index block

```
Index Block :=
  entry_count : u32 LE
  entries     : IndexEntry*    // one per data block, in block order (ascending key)
  checksum    : u32 LE

IndexEntry :=
  last_key_len  : u32 LE
  last_key      : [u8; last_key_len]
  block_offset  : u64 LE
  block_length  : u32 LE   // includes the block's own checksum trailer
```

Sparse index: one entry per data block, its highest key and file
location. A reader performs a binary search over `entries` (ascending
`last_key`) to find the first entry whose `last_key >= query_key` —
the only block that can contain `query_key`, since blocks are
non-overlapping and sorted (operating brief §16, §21).

**Resolved correctness subtlety, found by this phase's own property
tests, not guessed** (matches the LSM spec's own §2.9 test-checklist
item 7, "including the edge cases of... the last key of a block"):
since the writer closes a block purely on accumulated byte size, one
user key's version run **can** legitimately span two or more
consecutive blocks — every such block's `last_key` is then that same
key. Two consequences, both implemented and exercised by
`src/sstable/tests.rs`'s property test:
1. `entries` is only **non-decreasing** by `last_key`, not strictly
   ascending — `decode_index_block` accepts consecutive equal values
   and rejects only an actual decrease (which can never happen from a
   correct writer).
2. A reader must locate the **leftmost** index entry with
   `last_key >= query_key` — `Vec::partition_point`, never
   `Vec::binary_search_by`, which is explicitly permitted to return
   *any* matching index on a tie and would silently skip earlier
   blocks that also hold real versions of the query key. This exact
   bug was caught by `sstable::tests::property::
   sstable_matches_memtable_reference` during this phase's own
   implementation (a `binary_search_by`-based first draft returned
   `None` for a key whose oldest version lived in an earlier block of
   a same-`last_key` run) — recorded here so it is never
   reintroduced. Having found the leftmost match, the reader then
   extends forward while `last_key` keeps repeating exactly (plus one
   more block past the run, which might start with the tail of it),
   scans every candidate block, and keeps the highest `seq <=
   as_of_seq` found across all of them.

### 2.7 Footer (fixed 72 bytes)

| Field | Type | Bytes | Offset |
|---|---|---|---|
| `magic` | `[u8;8]` = `"RBXSST01"` | 8 | 0 |
| `format_version` | `u32` LE = `1` | 4 | 8 |
| `min_seq` | `u64` LE | 8 | 12 |
| `max_seq` | `u64` LE | 8 | 20 |
| `record_count` | `u64` LE (total across all data blocks) | 8 | 28 |
| `bloom_offset` | `u64` LE | 8 | 36 |
| `bloom_length` | `u64` LE | 8 | 44 |
| `index_offset` | `u64` LE | 8 | 52 |
| `index_length` | `u64` LE | 8 | 60 |
| `footer_crc32c` | `u32` LE (CRC32C over bytes `[0, 68)`) | 4 | 68 |

`8+4+8+8+8+8+8+8+8+4 = 72 = FOOTER_SIZE`, a compile-time constant.
Fully accounted for — **no reserved/padding bytes in v1** (matches
`RUBIC_FORMAT_SPECIFICATION.md` §3.12's own note that this footer has
none today). A file shorter than 72 bytes, or whose `magic` or
`footer_crc32c` don't check out, is corrupt.

### 2.8 Build process (`write_from_memtable`)

1. Iterate the (frozen) MemTable's `range(..)` — already `(key asc,
   seq asc)`, no sort needed.
2. Buffer records into the current block; once at/over
   `target_block_size`, finalize it (`record_count`, records, CRC32C
   trailer) and record its `IndexEntry`.
3. Add every record's key to the bloom filter as it is processed.
4. After the last block: write the bloom filter block, then the index
   block, then the footer.
5. The entire sequence is written to a temporary file (Section 3) and
   becomes a real SSTable only via the atomic-rename discipline there
   — the build process never writes directly to a path a reader might
   treat as live.

### 2.9 Configured limits (operating brief §7, resolved explicitly)

| Limit | Value | Enforced |
|---|---|---|
| `target_block_size` | 4096 bytes (configurable, `LsmConfig::sstable_target_block_size`) | Soft — see §2.4 |
| `max_key_size` | 64 KiB (`u32`-representable, but bounded well below `u32::MAX` to keep a single key from being able to dominate a block or force pathological allocation sizes) | Hard — `EngineError::CapacityExceeded { requested, max }`, checked before allocation, on both write and read |
| `max_value_size` | 64 MiB — exactly `wal::DEFAULT_MAX_RECORD_LEN` (no reason for an SSTable record to admit a payload the WAL itself would already have rejected before it could ever reach a MemTable) | Hard — `EngineError::CapacityExceeded { requested, max }`, same enforcement point |
| Max blocks per file | Bounded only by `index_offset`/`block_offset` being valid `u64` values and `entry_count`/`record_count` being valid `u32`/`u64` values — no additional artificial cap | N/A |
| Max file size | Bounded only by `u64` offsets — no additional artificial cap this phase | N/A |

`max_key_size`/`max_value_size` are enforced **before** the length
prefix is used to allocate a read buffer (operating brief §45's
"validate before allocation") — a corrupt or adversarial `key_len`/
`value_len` field read from disk is checked against these constants
before any `Vec::with_capacity`/`read_exact` call sized by it.

### 2.10 Read API surface (matches the spec's §2.8, Phase-4B naming)

```rust
pub struct SstableMeta {
    pub id: u64,
    pub min_seq: u64,
    pub max_seq: u64,
    pub path: PathBuf,
}

pub struct SsTable { /* meta, file handle, bloom, index — bounded, not-fully-materialized */ }

pub enum RecordValue { Put(Vec<u8>), Tombstone }

impl SsTable {
    pub fn open(path: &Path, id: u64) -> Result<Self>; // reads footer, bloom, index eagerly; data blocks read lazily
    pub fn get_versioned(&self, key: &[u8], as_of_seq: u64) -> Result<Option<(u64, RecordValue)>>;
    pub fn range_scan_raw(&self, start: Bound<&[u8]>, end: Bound<&[u8]>)
        -> impl Iterator<Item = Result<(Vec<u8>, u64, RecordValue)>>;
}
```

`get_versioned` algorithm (unchanged from the LSM spec): bloom filter
check (definite miss -> `None`, zero I/O) -> binary search index for
the candidate block -> read + checksum-verify that block -> linear
scan its records for `key`, tracking the highest `seq <= as_of_seq` ->
return that record or `None`.

---

## 3. Phase-4B decisions with no Manifest (operating brief §27-§31, §52)

The user has selected, explicitly: **SSTable is a read-only addition
to the read path this phase; the WAL is never purged or truncated by a
flush, and recovery continues to perform the exact full
`wal::replay_streaming` reconstruction Phase 4A already does,
unchanged.** Every decision below is a direct consequence of that
choice.

### 3.1 SSTable IDs — directory-scan-based, not Manifest-based

**PHASE-4B DECISION.** `RubixDB-LSM-Engine-Specification-v1.0.md` §3.1
recovers `next_sstable_id` as `max(id seen in ADD_SSTABLE manifest
edits) + 1`. With no Manifest, `LsmEngine::open` instead scans
`<data_dir>/sstables/` for files matching the naming convention
(Section 3.2), parses each one's 20-digit decimal ID, and seeds
`next_sstable_id = max(ids found) + 1` (or `1` for an empty/absent
directory). This is not a "conflicting global ID system" (operating
brief §27's concern) — it produces the identical numbering a
Manifest-based scheme would produce from the same directory contents,
and a future Manifest phase can seed itself the same way from an
existing Phase-4B-created directory with zero migration step. IDs are
allocated via an in-process `AtomicU64`, fetch-added once per flush;
directory rescans never happen mid-session (only at `open()`), so two
concurrent flushes in one process can never collide.

### 3.2 File naming (matches the spec exactly — not a new decision)

```
<data_dir>/sstables/
  00000000000000000001.sst
  00000000000000000002.sst
  00000000000000000002.sst.tmp   <- build in progress, not yet visible
```

20-digit, zero-padded, decimal `sstable_id`, `.sst` extension for a
published table, `.sst.tmp` for a build in progress. Deterministic,
collision-safe (IDs are never reused — Section 3.1's monotonic
counter never rewinds within a process lifetime, and a fresh `open()`
always resumes above every ID it finds on disk), platform-safe (no
character outside `[0-9.]`, valid on both NTFS and POSIX filesystems),
version-compatible (the file's own footer `format_version` is the
compatibility signal, never the filename). An existing `.sst` file at
a given ID is never overwritten — a build always targets `{id}.sst.tmp`
first (a fresh, never-before-used ID, per Section 3.1), and the final
`rename` target is only ever written by that same build.

### 3.3 Publication / discovery without a Manifest

**PHASE-4B DECISION.** `LsmEngine::open`'s startup sweep of
`<data_dir>/sstables/`:

1. Delete every `*.sst.tmp` file unconditionally (identical rule to
   the already-final spec's own Section 3.3 — always an interrupted
   build, never anything a reader should trust, Manifest or not).
2. For every remaining `*.sst` file: open it and validate its footer
   (magic, `format_version`, `footer_crc32c`) per Section 2.7.
   - Valid: it is part of the live set. (There is no `REMOVE_SSTABLE`
     concept this phase — Compaction is out of scope, operating brief
     §53 — so "exists and validates" is a complete and correct
     substitute for Manifest-tracked liveness at Phase 4B's scope: no
     mechanism in this phase ever produces a `.sst` file that is
     durable-and-complete but *should not* be considered live.)
   - Invalid (bad magic / unsupported version / bad footer checksum /
     file shorter than `FOOTER_SIZE`): **`LsmEngine::open` returns
     `Err`, refusing to start**, rather than silently excluding the
     file from the live set. Justification (Section 3.5 below) is why
     this is safe to do without risking availability the WAL could
     have otherwise provided: correctness has already been decided in
     favor of never trusting an unvalidated file (operating brief §19,
     §47), and this phase deliberately has no lesser-severity response
     available (no partition-level `DEGRADED`, per
     `RubixDB-LSM-Engine-Specification-v1.0.md` §7.4's own note that
     this granularity doesn't exist yet) — the honest, fail-closed
     choice given only two states ("fully open" or "refuse to open")
     is to refuse rather than silently serve an incomplete database.
     An operator can always delete or move aside the offending `.sst`
     file to restore availability (Section 3.5 explains why this loses
     no durable data), which is recorded here as the explicit recovery
     procedure rather than left implicit.
3. This sweep never writes an `ADD_SSTABLE`-equivalent record anywhere
   — there is nothing to write it *to* this phase. Liveness is
   determined fresh, from disk, on every `open()`.

### 3.4 Atomic visibility state machine (operating brief §29)

```
NOT VALID  (bytes not yet written)
   |
   v
BUILDING   (writing to {id}.sst.tmp)
   |
   v
VALIDATED  (fully written, fsynced; not yet renamed)
   |
   v
PUBLISHED  (renamed to {id}.sst, containing directory fsynced)
```

Only `PUBLISHED` files (i.e., files bearing the final `.sst` name,
found by the Section 3.3 sweep, or appended to the in-memory
`sstables` list immediately after a successful publish within the
same process) are ever added to `LsmEngine`'s `sstables` list. A
duplicate ID can never arise (Section 3.1); an unknown future
`format_version` is rejected per Section 2.1/3.3, never guessed at.

### 3.5 Why "no purge" makes SSTable corruption a fail-closed *availability* concern, never a *durability* one

This is the load-bearing consequence of the user's Section 3-opening
decision, stated explicitly rather than left implicit: because the WAL
is **never** truncated or purged as a result of any flush this phase,
every record that was ever written is still fully present and
replayable from the WAL, unconditionally, regardless of whether any
SSTable exists, is missing, or is corrupt. A corrupt or missing `.sst`
file can therefore never cause **data loss** in Phase 4B — at worst it
causes `LsmEngine::open` to refuse to start (Section 3.3) until an
operator removes the offending file, after which the engine opens with
one fewer read-path SSTable and a `MemTable` that (via full WAL
replay, unchanged from Phase 4A) still contains every record that
SSTable ever held. This is explicitly *not* the case once a future
Manifest/purge phase exists — this property is a direct, temporary
consequence of this phase's specific WAL-untouched decision, and must
be re-examined the moment WAL purging is introduced.

### 3.6 Flush does not shrink WAL retention (explicit non-goal, not an oversight)

**PHASE-4B DECISION (direct consequence of the chosen option).** A
successful flush's only durability-relevant effect is: the data is now
also durably present in a published SSTable. It does **not**:
- call `wal::purge_before` (that primitive exists, tested at the WAL
  layer since Phase 3C, but Phase 4B adds no caller of it),
- change `wal::replay_streaming`'s behavior or its callers,
- change what `LsmEngine::open` replays (still the entire WAL, exactly
  as Phase 4A already does).

Its only in-memory effect is allowing the now-flushed `Arc<MemTable>`
to be dropped from `immutables` (freeing that RAM during normal
operation, per operating brief §44) once the writer has fully and
durably published its SSTable. WAL growth and full-replay cost are
therefore **unbounded across restarts within Phase 4B's scope** — an
explicit, named limitation, not a regression (Phase 4A already had
unbounded WAL growth; Phase 4B does not make this better yet, and does
not pretend to). Bounding it is the Manifest phase's job.

---

## 4. Windows/Linux durability differences (operating brief §9)

The atomic-construction discipline
(`RubixDB-LSM-Engine-Specification-v1.0.md` §3.2) is: write to
`{id}.sst.tmp` -> `fsync` the temp file -> `rename` to `{id}.sst` ->
`fsync` the containing directory. Phase 4B reuses
`wal::file_io::fsync_dir` (re-exported `pub(crate)` from `wal::mod`
for this purpose — Section 5 below) rather than writing a second,
divergent platform-specific implementation:

- **Unix**: `fsync_dir` opens the directory and calls `sync_all()` —
  a real, effective fsync of the directory's own metadata, making the
  preceding `rename` durable across a crash.
- **Windows**: `fsync_dir` is a **documented no-op** (see
  `src/wal/file_io.rs`'s own doc comment on `fsync_dir`, carried
  forward unchanged here) — Windows has no directly reachable
  equivalent from safe, dependency-free `std` code, and NTFS's own
  metadata journaling makes the failure mode this guards against
  (a durable rename whose directory-entry update itself did not
  survive a crash) considerably less likely in practice than on a
  filesystem like ext4 with delayed allocation. This project's
  Windows development/test environment therefore has a real, named gap
  here: on Windows, a crash in the narrow window between a successful
  `rename` and this (no-op) "fsync" could in principle still lose the
  rename's own directory-entry update on some filesystem/hardware
  combinations, even though `File::rename` itself is atomic with
  respect to *concurrent readers* on both platforms. This gap already
  exists, identically, for WAL segment rotation/purge on this same
  platform — Phase 4B introduces no new instance of it, only reuses
  the already-accepted one.
- `File::rename` (`std::fs::rename`) is atomic with respect to
  concurrent observers on both NTFS and POSIX filesystems (a reader
  never observes a half-renamed file), the property Section 3.4's
  state machine actually depends on for correctness *within a single
  running process or a clean restart*; the fsync-of-directory step
  above is specifically about surviving a **crash**, a strictly
  narrower and platform-dependent guarantee, named separately here so
  the two are never conflated.

---

## 5. Required additive change to `src/wal/mod.rs`

`fsync_dir` (`src/wal/file_io.rs`) is `pub(crate)` but unreachable
outside `wal` today because `mod file_io;` itself is private. Phase 4B
adds exactly one line, `pub(crate) use file_io::fsync_dir;`, to
`src/wal/mod.rs` — a visibility-only change. It does not alter
`fsync_dir`'s behavior, signature, or any WAL/GroupCommitter/
BatchCoordinatorPool code path (operating brief §3's preservation
instruction is about behavior; this is purely a module-privacy export
enabling reuse instead of duplication of already-tested,
platform-specific durability code, which is the more conservative
choice per §9's "follow the project's existing WAL lesson").

---

## 6. Corruption classification (operating brief §33-§34, applying `RUBIC_FORMAT_SPECIFICATION.md` §3.11)

Unlike a WAL segment (which can have a legitimate trailing torn write
from an in-progress append), a discovered `.sst` file can **never**
legitimately be torn: it only ever bears its final name after the
atomic-rename step (Section 3.4), which never runs until the entire
file — every data block, the bloom filter block, the index block, and
the footer — has already been fully written and fsynced. Therefore:

**Every corruption found in a file bearing the `.sst` extension is
real corruption, never a torn write, with no exception.** This is a
strictly simpler rule than the WAL's own torn-vs-corrupt distinction
(WAL Spec §6.3) — there is no "last frame" special case here at all,
because there is no legitimate in-progress state a `.sst`-named file
can ever be found in.

| Corruption | Classification | Reader behavior |
|---|---|---|
| Bad footer magic | Corruption | `open()` returns `Err(EngineError::Corruption{detail: "footer: bad magic"})` |
| Unrecognized `format_version` | Unsupported version, not corruption | `open()` returns `Err(EngineError::Unsupported{operation: "sstable format_version N"})` |
| Bad `footer_crc32c` | Corruption | `open()` returns `Err(EngineError::Corruption{detail: "footer: checksum mismatch"})` |
| File shorter than 72 bytes | Corruption | `open()` returns `Err(EngineError::Corruption{detail: "footer: file shorter than FOOTER_SIZE"})` |
| Bad index checksum | Corruption | `open()` returns `Err(EngineError::Corruption{detail: "index: checksum mismatch"})` |
| Bad bloom checksum | Corruption | `open()` returns `Err(EngineError::Corruption{detail: "bloom: checksum mismatch"})` |
| Index/bloom offset or length out of file bounds | Corruption | `open()` returns `Err(EngineError::Corruption{detail: "index/bloom: offset out of file bounds"})` |
| Bad data-block checksum | Corruption | `get_versioned`/iteration returns `Err(EngineError::Corruption{detail: "block N: checksum mismatch"})` for that access — detected lazily, since blocks are read on demand (Section 2.10), never eagerly scanned at `open()` |
| Malformed record (`op` not in `{1,2}`, length field pointing past block end) | Corruption | `Err(EngineError::Corruption{detail: "record: malformed"})` at the point the record is decoded |
| Overlapping blocks (an `IndexEntry`'s range overlaps another's) | Corruption | Detected during index validation at `open()`: `Err(EngineError::Corruption{detail: "index: overlapping blocks"})` |
| Oversized `key_len`/`value_len` read from disk | Rejected before allocation, not "corruption" in the checksum sense, but equally fail-closed | `Err(EngineError::CapacityExceeded{requested, max})` |

No case above is ever silently skipped, best-effort-repaired, or
treated as "expected." Per Section 3.5, none of these ever risk data
loss this phase — they risk *availability* of that one file only.

---

## 7. What remains UNDEFINED / deferred (not guessed)

- Per-field "feature flag" bits for forward compatibility
  (`RUBIC_FORMAT_SPECIFICATION.md` §3.9) — undefined, no motivating
  case yet.
- Multiple independently-versioned metadata sections within one file
  (`RUBIC_FORMAT_SPECIFICATION.md` §3.8) — undefined, no motivating
  case yet.
- A future extension-block mechanism (`RUBIC_FORMAT_SPECIFICATION.md`
  §3.13), e.g. for per-block compression — explicitly deferred by the
  LSM spec itself (§2.3) pending real benchmark evidence.
- Renaming `"RBXSST01"`'s magic to something spelling out "RUBIC" —
  explicitly left to a separate, deliberate decision
  (`RUBIC_FORMAT_SPECIFICATION.md` §2); **not done in Phase 4B** — the
  magic stays exactly `"RBXSST01"`, matching the already-final spec
  byte-for-byte, because renaming it is not required for correctness
  and this document's own governing principle (`RUBIC_FORMAT_
  SPECIFICATION.md` §1's "do not silently rename") applies here with
  full force even though no code existed yet to "rename."
- Manifest-based SSTable ID/liveness tracking, WAL purge-on-flush,
  compaction, and everything Section 3 above explicitly stands in for
  without being one — all deferred to their own future phases,
  per operating brief §52-§53.
