# RubixDB LSM Engine Specification v1.0

Status: Final — ready for implementation
Component: Memtable, SSTable, Manifest, LSM Engine Facade, Compaction, Recovery (Phase 0, Steps 2–6)
Implements: RubixDB Architecture Specification v1.0 (Sections 4, 5, 7); builds directly on RubixDB WAL Specification v1.0 (Phase 0, Step 1)

---

## 0. Purpose and Scope

This document completes the Phase 0 spec set. The WAL Specification v1.0 covers Step 1; this document covers Steps 2 through 6 — Memtable, SSTable, the Manifest (the piece the WAL spec deliberately left for here — see 0.1), the LSM engine facade that implements the Architecture Spec's Storage Engine Contract, Compaction, and the full end-to-end Recovery procedure that ties WAL + Manifest + SSTables + Memtable together. Together, the two documents specify the entire "correct, tested, benchmarked LSM engine" that is Phase 0's deliverable — nothing here depends on the router, cost model, metadata manager, or any second engine.

### 0.1 The Manifest: closing a gap deliberately left open until now

An LSM engine needs a durable answer to the question "which SSTable files are currently valid?" that survives a crash mid-flush or mid-compaction — otherwise a reader can pick up a half-written or already-superseded file. Section 6 defines the **Manifest**, a small append-only log (reusing the WAL's exact frame format from WAL Spec §2.3) that is the single source of truth for the live SSTable set. This is intentionally distinct from the Architecture Spec's partition-level Metadata Manager (Architecture Spec §6) — the Manifest is engine-local, single-instance bookkeeping that exists purely for Phase 0's single, unpartitioned engine; it does not survive as-is into Phase 1, where the Metadata Manager takes over ownership tracking at the partition level.

### 0.2 Conventions carried over from the WAL spec

CRC32C (Castagnoli) for all checksums, via the same `crc32c` crate. All multi-byte integers are little-endian. `op` byte values `1 = PUT`, `2 = DELETE` are identical to WAL Spec §2.4 — this is deliberate, not a coincidence, and no translation layer exists between the two. "Torn write" vs. "corruption" is determined by the identical rule as WAL Spec §6.3 (only the trailing incomplete frame of the last-written file can be a torn write; anything else is corruption and must be escalated, never silently discarded) — this rule applies to the Manifest exactly as it applies to the WAL, since the Manifest uses the same frame format.

---

## 1. Memtable

### 1.1 Data structure

A single in-memory sorted structure, keyed by `(user_key, seq)` ascending — **not** just `user_key`. This is required, not a simplification: until a memtable is flushed, it must be able to answer a snapshot read (`as_of_seq` in the past) that needs an *older* version of a key that was overwritten later in the same memtable. Collapsing to "latest value per key" during the memtable's own lifetime would silently break Architecture Spec §7.4's snapshot-read guarantee.

```rust
pub struct MemTable {
    map: BTreeMap<(Vec<u8>, u64), MemtableValue>,  // (user_key, seq) -> value, ascending
    size_bytes: usize,
    max_size_bytes: usize,
    min_seq: Option<u64>,
    max_seq: Option<u64>,
}

pub enum MemtableValue {
    Put(Vec<u8>),
    Tombstone,
}
```

Because `BTreeMap` orders tuples lexicographically, all versions of one `user_key` are contiguous and sorted by ascending `seq`. This single property is what makes every lookup below a bounded range operation rather than a full scan.

### 1.2 API

```rust
impl MemTable {
    pub fn new(max_size_bytes: usize) -> Self;

    /// Low-level, used both by normal writes and by WAL replay during recovery.
    pub fn insert(&mut self, key: &[u8], seq: u64, value: MemtableValue);

    pub fn put(&mut self, key: &[u8], seq: u64, value: &[u8]) {
        self.insert(key, seq, MemtableValue::Put(value.to_vec()));
    }
    pub fn delete(&mut self, key: &[u8], seq: u64) {
        self.insert(key, seq, MemtableValue::Tombstone);
    }

    /// Latest version as of "now" — equivalent to get_as_of(key, u64::MAX).
    pub fn get(&self, key: &[u8]) -> Option<(&u64, &MemtableValue)>;

    /// Highest seq <= as_of_seq for this key, or None if this memtable has
    /// no version of the key at or before as_of_seq.
    pub fn get_as_of(&self, key: &[u8], as_of_seq: u64) -> Option<(&u64, &MemtableValue)> {
        self.map
            .range((key.to_vec(), 0)..=(key.to_vec(), as_of_seq))
            .next_back()
    }

    /// Raw iteration, ALL versions, in (key asc, seq asc) order — version
    /// resolution and tombstone filtering are the caller's (LSM facade's)
    /// responsibility, not the memtable's. See Section 4.2.
    pub fn range(&self, start: Bound<&[u8]>, end: Bound<&[u8]>)
        -> impl Iterator<Item = (&(Vec<u8>, u64), &MemtableValue)>;

    pub fn size_bytes(&self) -> usize { self.size_bytes }
    pub fn is_full(&self) -> bool { self.size_bytes >= self.max_size_bytes }
    pub fn seq_range(&self) -> Option<(u64, u64)>;

    /// Consumes self and returns an immutably-shared handle. This is a
    /// compile-time guarantee, not a runtime flag: once frozen, there is no
    /// code path left in the type system that can mutate this memtable again.
    pub fn freeze(self) -> Arc<MemTable>;
}
```

`get_as_of`'s range-and-`next_back` pattern is the one piece of logic in this component worth being precise about: `range((key,0)..=(key,as_of_seq))` isolates exactly this key's versions with `seq <= as_of_seq` (because `(key, seq)` tuples for a fixed `key` are contiguous and ordered by `seq`), and `.next_back()` — the last element of that sub-range — is by construction the *highest* qualifying `seq`, i.e., the correct answer. No special-casing, no manual binary search.

### 1.3 Size accounting

`entry_size(key, value) = key.len() + value_payload_len + ENTRY_OVERHEAD`, where `ENTRY_OVERHEAD = 32` (a fixed, documented approximation for the `seq: u64`, enum discriminant, and `BTreeMap` node overhead — not exact, and not required to be; it only needs to be a stable, conservative estimate that keeps the configured `max_size_bytes` a meaningful trigger). `size_bytes` is incremented on every `insert` of a new `(key, seq)` pair. `insert` is never called twice with an identical `(key, seq)` pair in normal operation (WAL `seq` values are unique per key by construction); Recovery (Section 7) is what guarantees this holds during replay too.

### 1.4 Tombstones

A memtable never performs GC on tombstones — that only happens during Compaction (Section 5), which is the only place Architecture Spec §7.3's tombstone-safety rule (checked against `snapshot_refs`) is evaluated. A tombstone in a memtable is just another entry with `MemtableValue::Tombstone` and flushes to an SSTable like any other record.

### 1.5 Concurrency

A `MemTable` is **not** internally synchronized — same single-writer principle as the WAL (WAL Spec §9). The LSM facade (Section 4) is responsible for serializing writers and for exposing safe concurrent reads: a frozen memtable, wrapped in `Arc<MemTable>` by `freeze()`, is safe to share across threads precisely because nothing can mutate it anymore — Rust's ownership rules enforce this at compile time, not by convention.

### 1.6 Test checklist

1. Put N keys, get them all back with correct values.
2. Delete a subset; `get` returns `Tombstone` for them, not silently absent.
3. Multiple writes to the same key at increasing `seq`; `get_as_of` at each intermediate `seq` returns exactly the version that was current at that point, including the oldest one.
4. `get_as_of` with `as_of_seq` before the key's first write returns `None`.
5. `size_bytes()` grows by exactly `entry_size(...)` per insert and matches a hand-computed total after N inserts.
6. `is_full()` flips at the configured threshold, not before or after.
7. `range()` returns strictly sorted `(key, seq)` order across a mix of single- and multi-version keys.
8. `freeze()` compiles to a type that has no `&mut` methods reachable — assert this is a compile-time property (a test that *would* fail to compile if uncommented is an acceptable way to document this; do not write a runtime-only test for a compile-time guarantee).
9. Empty memtable: `get`, `get_as_of`, and `range` all behave correctly (no panics, correct `None`/empty results).

---

## 2. SSTable

### 2.1 File layout

```
+----------------------------------+
| Data Block 0                     |
| Data Block 1                     |
| ...                               |
| Data Block N-1                   |
+----------------------------------+
| Bloom Filter Block                |
+----------------------------------+
| Index Block                       |
+----------------------------------+
| Footer (fixed 72 bytes)           |
+----------------------------------+
```

The footer has a fixed size and sits at the very end of the file, so a reader opens the file, seeks to `file_len - 72`, and has everything needed to locate every other section — no separate metadata file is required to read a single SSTable in isolation (the Manifest, Section 6, is what tracks *which* SSTables currently exist, not how to read one you already have).

### 2.2 Data record

Records within a data block are sorted `(key ascending, seq ascending)` — the same ordering convention as the Memtable (Section 1.1), deliberately, so no reversal step exists anywhere in the build pipeline between the two.

```
Record :=
  key_len   : u32 LE
  key       : [u8; key_len]
  seq       : u64 LE
  op        : u8         // 1 = PUT, 2 = DELETE — identical values to WAL Spec §2.4
  value_len : u32 LE     // 0 when op == DELETE
  value     : [u8; value_len]
```

### 2.3 Data block

```
Block :=
  record_count : u32 LE
  records      : Record*     // record_count of them, concatenated
  checksum     : u32 LE      // CRC32C over (record_count bytes || all record bytes)
```

A new block is started once the current one reaches `target_block_size` (config, default **4096 bytes**) — this is a soft threshold: a record in progress is never split across a block boundary, so the last record of a block may push it slightly over the target. Prefix compression / restart points (as used in LevelDB-style formats) are explicitly **not** implemented in v1 — this is a deliberate scope cut, not an oversight, and is listed in Section 10 (Open Items) as a candidate optimization once real benchmark data (Architecture Spec §17) shows block-scan cost actually matters.

### 2.4 Bloom filter block

One filter per SSTable, covering every key in the file (not per-block — per-block filters are a possible future optimization, not required for Phase 0).

```
Bloom Filter Block :=
  num_bits            : u64 LE
  num_hash_functions  : u8
  bits                : [u8; ceil(num_bits / 8)]
  checksum            : u32 LE   // CRC32C over everything above
```

Parameters, pinned for v1: **10 bits per key**, giving `num_hash_functions = round(10 * ln(2)) = 7` and an expected false-positive rate near 1%. Hashing: two independent 64-bit hashes of the key via `XXH64` with seeds `0` and `1` (the `xxhash-rust` or `twox-hash` crate), combined by double hashing (Kirsch–Mitzenmacher): `bit_i(key) = (h1(key) + i * h2(key)) mod num_bits` for `i` in `0..num_hash_functions`. Every record's key is added to the filter once per occurrence (including repeated additions across multiple versions of the same key) — this is harmless (bloom filters are naturally idempotent under re-insertion of the same key) and avoids needing a separate dedup pass during the build.

A bloom filter answers "definitely absent" (skip this SSTable entirely, no I/O) or "maybe present" (must check the block) — it must never produce a false negative; a false positive only ever costs a wasted block read, never a correctness violation.

### 2.5 Index block

A sparse index: one entry per data block, giving the block's last (highest) key and its file location — this is what turns a point lookup into a binary search over blocks instead of a linear file scan.

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

### 2.6 Footer (fixed 72 bytes)

| Field | Type | Bytes |
|---|---|---|
| `magic` | `[u8;8]` = `"RBXSST01"` | 8 |
| `format_version` | `u32` LE = `1` | 4 |
| `min_seq` | `u64` LE | 8 |
| `max_seq` | `u64` LE | 8 |
| `record_count` | `u64` LE (total across all data blocks) | 8 |
| `bloom_offset` | `u64` LE | 8 |
| `bloom_length` | `u64` LE | 8 |
| `index_offset` | `u64` LE | 8 |
| `index_length` | `u64` LE | 8 |
| `footer_crc32c` | `u32` LE (CRC32C over every preceding footer byte) | 4 |

`8+4+8+8+8+8+8+8+8+4 = 72`. `FOOTER_SIZE = 72` is a compile-time constant; any file shorter than 72 bytes, or whose `magic`/`footer_crc32c` don't check out, is corrupt (Section 7 handles what that means for recovery — an SSTable is either fully valid or it does not exist as far as the engine is concerned; see Section 3.3's atomic-build discipline for why a partially-written one should never be observable under its final name in the first place).

### 2.7 Build process (`write_from_memtable`)

1. Iterate the memtable's `range(..)` (Section 1.2) — already in `(key asc, seq asc)` order, no sort needed.
2. Buffer records into the current block; once it reaches `target_block_size`, finalize it (write `record_count`, the records, and the CRC32C trailer) and record its `IndexEntry`.
3. Add every record's key to the bloom filter as it's processed (Section 2.4).
4. After the last block, write the bloom filter block, then the index block, then the footer.
5. This entire sequence is written to a **temporary file** (Section 3) and only becomes a real SSTable via the atomic-rename discipline defined there — the build process itself never writes directly to a file the engine might treat as live.

### 2.8 Read API

```rust
pub struct SSTableMeta {
    pub id: u64,
    pub min_seq: u64,
    pub max_seq: u64,
    pub path: PathBuf,
}

pub struct SSTable {
    meta: SSTableMeta,
    file: File,          // or a memory-mapped view; see Section 10
    bloom: BloomFilter,
    index: Vec<IndexEntry>,
}

pub enum RecordValue { Put(Vec<u8>), Tombstone }

impl SSTable {
    pub fn open(meta: SSTableMeta) -> Result<Self>; // reads footer, bloom, index eagerly; data blocks read lazily on demand

    /// Highest version of `key` with seq <= as_of_seq found IN THIS FILE, or
    /// None if this file has nothing for `key` at or before as_of_seq. This
    /// is the raw, per-source primitive the LSM facade merges across
    /// (Section 4.2) — it deliberately does NOT collapse Tombstone to None;
    /// that collapse happens once, at the very end of the facade's merge.
    pub fn get_versioned(&self, key: &[u8], as_of_seq: u64) -> Result<Option<(u64, RecordValue)>>;

    /// Ordered iteration of (key, seq, RecordValue) across [start, end),
    /// raw (all versions this file has in range) — merging across sources
    /// and version resolution again belongs to the LSM facade.
    pub fn range_scan_raw(&self, start: Bound<&[u8]>, end: Bound<&[u8]>)
        -> impl Iterator<Item = Result<(Vec<u8>, u64, RecordValue)>>;
}
```

`get_versioned` algorithm: (1) check the bloom filter — a definite miss returns `None` with zero block I/O; (2) binary search `index` for the first entry whose `last_key >= key` — that block is the only one that can contain `key`, since blocks are non-overlapping and sorted; (3) read and checksum-verify that block; (4) linear-scan its (typically small) run of records for this exact `key`, tracking the highest `seq <= as_of_seq` seen; (5) return that record, or `None` if the block held no qualifying version (a legitimate bloom-filter false positive, or the only versions present are newer than `as_of_seq`).

### 2.9 Test checklist

1. Build an SSTable from a memtable with N keys, multiple versions of some; reopen and verify every version is retrievable via `get_versioned` at the right `as_of_seq`.
2. Bloom filter rejects a large sample of keys known not to be in the file, with false-positive rate within a reasonable bound of the configured target (statistical test, not exact).
3. Bloom filter **never** rejects a key that IS in the file (zero false negatives, exhaustively checked over every key actually written).
4. `range_scan_raw` returns strictly sorted `(key, seq)` results matching a scan of the source memtable.
5. Footer round-trip: every field written is read back identically; a deliberately corrupted `footer_crc32c` is detected and rejected.
6. A corrupted byte inside a data block (not the footer) is detected via that block's own checksum, not silently returned as valid data.
7. Index binary search correctness across block boundaries — including the edge cases of the first key of a block and the last key of a block.
8. Large SSTable (enough keys to span hundreds of blocks) — verify the index has exactly one entry per block and lookups remain correct throughout.
9. `open()` on a file smaller than `FOOTER_SIZE` returns `Corruption`, not a panic.

---

## 3. Atomic SSTable Construction

This section is short and is the single most important correctness property of the whole component: **an SSTable file is either fully valid under its final path, or it does not exist at that path at all.** There is no third state an SSTable-consuming reader can ever observe.

### 3.1 Directory layout

```
<data_dir>/sstables/
  00000000000000000001.sst
  00000000000000000002.sst
  00000000000000000002.sst.tmp   ← build in progress, not yet visible
```

`sstable_id` is a separate monotonic counter from the WAL's `segment_id`, recovered on startup as `max(id seen in ADD_SSTABLE manifest edits) + 1` (Section 6).

### 3.2 Build sequence

1. Write the complete SSTable (Section 2.7) to `sstables/{id}.sst.tmp`.
2. `fsync` the temp file.
3. `rename(tmp_path, final_path)` — atomic on POSIX filesystems.
4. `fsync` the containing directory (`sstables/`) — required on most POSIX filesystems for the rename itself to be durable across a crash, not just the file contents.
5. Only now, append an `ADD_SSTABLE` edit to the Manifest (Section 6) and `fsync` it.

Step ordering matters and is not arbitrary: the file must be durable on disk (steps 1–4) strictly before the Manifest claims it exists (step 5). The reverse order would let a crash produce a Manifest entry pointing at a file that was never actually made durable.

### 3.3 What a crash at each step leaves behind

- Crash during step 1–2: an orphaned `.sst.tmp` file, no Manifest reference. Recovery (Section 7) deletes all `.sst.tmp` files unconditionally on startup — they are always build-in-progress artifacts, never anything a reader should trust.
- Crash during step 3–4: either the rename didn't happen (still a `.tmp` file, same as above) or it did and is durable (indistinguishable from full success at this point, and that's fine — the file is valid either way; only the Manifest entry is still missing, handled by the orphan sweep below).
- Crash during/after step 5 but before its `fsync`: the file is valid and durable, but the Manifest may or may not reflect it. If it doesn't, recovery's orphan sweep (Section 7.2) finds a valid `.sst` file with no `ADD_SSTABLE` entry and re-adds it (re-appending the same `ADD_SSTABLE` edit is safe and idempotent — it just describes a file that's already correctly on disk).

---

## 4. LSM Engine Facade

This is the component that implements the Architecture Spec's Storage Engine Contract (§4.1) on top of Sections 1–3.

### 4.1 Structure

```rust
pub struct LsmEngine {
    wal: Wal,
    manifest: Manifest,
    active: RwLock<MemTable>,
    immutables: RwLock<VecDeque<Arc<MemTable>>>,   // newest first
    sstables: RwLock<Vec<Arc<SSTable>>>,           // newest first
    next_sstable_id: AtomicU64,
    config: LsmConfig,
}

pub struct LsmConfig {
    pub memtable_max_size_bytes: usize,       // default 4 MiB
    pub sstable_target_block_size: usize,     // default 4096 bytes (Section 2.3)
    pub bloom_bits_per_key: u32,              // default 10 (Section 2.4)
    pub compaction_trigger_count: usize,      // default 4 (Section 5)
}
```

### 4.2 Read path: the ReadView pattern

Every `get`, `range_scan`, and `snapshot` call begins by acquiring the three `RwLock`s just long enough to **clone the `Arc` pointers** (the active memtable is itself wrapped for this purpose — see note below) into a local, immutable `ReadView { active: Arc<MemTable>, immutables: Vec<Arc<MemTable>>, sstables: Vec<Arc<SSTable>> }`, then releases every lock and does all actual work against that `ReadView`. This is the resolution to a concurrency gap identified during this project's design review: a concurrent flush or compaction can freely swap the *live* pointers after a `ReadView` is captured without affecting a read already in progress, because the read is working against `Arc`-shared, individually-immutable snapshots of each component (a frozen `MemTable` never mutates again per Section 1.5; an `SSTable` is immutable by construction per Section 3). The *active* memtable is the one mutable piece — captured as an `Arc<RwLock<MemTable>>`-style clone-then-read-lock for the specific keys/ranges a read needs, held only as long as that one memtable's portion of the read takes, not for the whole multi-source merge.

`get(key)`: build a `ReadView`, then check `active`, then each `immutables` entry newest-to-oldest, then each `sstables` entry newest-to-oldest (bloom-filter short-circuited); return the value from the **first source with any version at `seq <= now`** — sources are consulted in strict recency order specifically so the first hit is guaranteed to be the authoritative one and older sources never need to be checked. If that first hit is `RecordValue::Tombstone`, the public `get` returns `None` (Architecture Spec §4.1's collapse rule); if it is `Put(v)`, return `Some(v)`.

`range_scan(start, end, as_of_seq)`: k-way merge the `range` (Section 1.2) / `range_scan_raw` (Section 2.8) iterators of every source in the `ReadView` covering `[start, end)`, keyed by `(user_key asc, source recency desc)`; for each distinct `user_key`, take the highest `seq <= as_of_seq` across all sources, apply the tombstone collapse, and yield `Put` results only.

`snapshot()`: captures a `ReadView` plus `as_of_seq = current max seq`, and registers itself so Compaction (Section 5) knows not to drop any tombstone or version this snapshot might still need — this is the Phase 0, single-engine realization of Architecture Spec §6.1's `snapshot_refs` and §7.3's tombstone-safety rule.

### 4.3 Write path

```
put/delete
  → wal.append_sync(...)              // WAL Spec §4–5; returns WalPosition{seq, ...}
  → active.write().insert(key, seq, value)
  → if active.is_full(): freeze_and_flush()  // Section 4.4
```

`put`/`delete` are serialized by a single writer path (a write-side `Mutex` around the append-then-insert sequence, or simply requiring the caller to serialize — Phase 0 is single-writer, per the project's incremental build principle; see the Agentic Build Prompt's Tier 3 note on not introducing concurrency machinery the current phase doesn't need).

### 4.4 Freeze and flush

```
freeze_and_flush():
  1. old = active.write().take_and_replace_with(MemTable::new(...))  // swap in a fresh memtable
  2. frozen = old.freeze()                                            // Arc<MemTable>, Section 1.2
  3. immutables.write().push_front(frozen.clone())
  4. (background) id = next_sstable_id.fetch_add(1)
     meta = SSTable::write_from_memtable(&frozen, id, data_dir)       // Section 2.7 + Section 3
     sstables.write().insert(0, Arc::new(SSTable::open(meta)?))
     wal.append_sync(WalOp::CheckpointMarker { flushed_through_seq: frozen.seq_range().unwrap().1 })
     manifest.append(ManifestEdit::SetCheckpoint { flushed_through_seq, wal_position })
     immutables.write().retain(|m| !Arc::ptr_eq(m, &frozen))
     wal.purge_before(flushed_through_seq)                            // WAL Spec §10
```

Step 1's swap must be atomic with respect to concurrent writers — implemented as a single critical section under the `active` lock's write guard, not as two separate operations.

### 4.5 Test checklist

1. `put`/`get`/`delete` round trip through the full facade (not just the memtable) — including after a flush has moved data into an SSTable, to confirm the read path correctly finds it there.
2. Multi-source version resolution: write the same key across active memtable, an immutable memtable, and an SSTable at three different `seq`s; confirm `get` returns the newest and `get`-as-of (via `snapshot`) returns the right older one.
3. Flush triggers automatically at `memtable_max_size_bytes` and the resulting SSTable is immediately queryable.
4. A `snapshot()` taken before a compaction continues to see pre-compaction data correctly even after the compaction completes and the old SSTables are gone from the live set.
5. Concurrent readers during an in-progress flush observe either the fully-pre-flush or fully-post-flush state for any single read — never a torn mix (this is the `ReadView` guarantee from Section 4.2, and is the test that would fail if that pattern were implemented incorrectly).
6. Restart mid-write (crash injected between WAL `append_sync` and memtable `insert`) — Recovery (Section 7) reconstructs the missing memtable entry from the WAL and the key is present after restart.
7. `range_scan` across active + immutable + multiple SSTables returns correctly merged, correctly ordered, tombstone-filtered results.

---

## 5. Compaction

### 5.1 Trigger and strategy

Size-tiered, deliberately simple for v1: whenever the live SSTable count reaches `compaction_trigger_count` (default 4), compact **all** currently-live SSTables into a single new one ("full compaction" rather than a partial/leveled selection). This is a documented scope cut, not an oversight — Architecture Spec §17's ablation/benchmark protocol is what determines whether a more selective strategy is actually worth the added complexity; guessing at that answer now would violate this project's "calibrate from measurement" principle.

### 5.2 Algorithm

```
compact(inputs: Vec<Arc<SSTable>>) -> Result<SSTableMeta>:
  1. new_id = next_sstable_id.fetch_add(1)
  2. k-way merge inputs' range_scan_raw(..) over the full key space, in (key asc, seq asc) order
  3. for each distinct key's version run across all inputs:
       - determine the surviving versions per the Architecture Spec §7.3 tombstone-safety rule:
         a version (including a tombstone) may be dropped only if no live snapshot_ref
         (Section 4.2) has an as_of_seq that could still need it, AND it is superseded
         by a newer surviving version of the same key
       - versions that survive are written, in order, to the output stream
  4. build the output as a new SSTable via the identical process to Section 2.7 (same block/bloom/index/footer construction) and the identical atomic-rename discipline (Section 3)
  5. manifest.append(ADD_SSTABLE(new_id)), fsync
  6. for each input: manifest.append(REMOVE_SSTABLE(input.id)), fsync
  7. sstables.write(): insert new, remove inputs (matching the manifest's now-durable state)
  8. delete the input .sst files from disk once no in-flight reader holds a ReadView referencing them (simplest v1 implementation: refcount via Arc — an Arc<SSTable> whose strong_count drops to 1, i.e., only the sstables list itself holds it, after being removed from that list, is safe to unlink)
```

Step 5 happening before step 6 is deliberate and, unlike the SSTable-vs-Manifest ordering in Section 3.2, is **not** load-bearing for correctness here: because the new SSTable is a complete, self-sufficient, correctly version-resolved merge of the inputs, a crash between steps 5 and 6 leaves the old input SSTables redundantly still "live" in the Manifest alongside the new one. Reads remain correct either way (recency-ordering still finds the right answer regardless of which redundant copy is consulted), and Recovery's Manifest replay (Section 7) simply catches up the remaining `REMOVE_SSTABLE` edits — or, if they were never issued at all before the crash, a subsequent compaction cycle will naturally subsume the stale duplicates. This is worth stating explicitly because it is the kind of ordering question that's easy to get wrong by assuming every ordering decision is as strict as Section 3.2's.

### 5.3 Test checklist

1. Trigger compaction via `compaction_trigger_count`; verify the resulting single SSTable contains the correct merged, version-resolved data compared to querying the original inputs directly.
2. A tombstone with no outstanding snapshot referencing anything older is dropped by compaction; verify the deleted key is genuinely gone from the compacted output.
3. A tombstone with an active `snapshot()` still open (from before compaction) is retained; verify the snapshot's view is unaffected by the compaction that just ran.
4. Crash injected between manifest step 5 (`ADD_SSTABLE` new) and step 6 (`REMOVE_SSTABLE` old) — restart and verify reads are still correct (both old and new temporarily live is acceptable; wrong or missing data is not).
5. Disk usage after compaction is bounded relative to before (verifies redundant/obsolete versions were actually dropped, not just relocated).
6. Concurrent reads during an in-progress compaction never see a partially-written new SSTable (guaranteed structurally by Section 3, but test it directly against this component too).

---

## 6. Manifest

### 6.1 Format

Reuses the WAL's exact frame format (WAL Spec §2.3): `length: u32 LE, crc32c: u32 LE, body: [u8; length]`, stored in a single append-only file at `<data_dir>/MANIFEST`. `body := edit_type(1 byte) || type_fields`.

| `edit_type` | Name | `type_fields` |
|---|---|---|
| `1` | `ADD_SSTABLE` | `sstable_id:u64, min_seq:u64, max_seq:u64, file_size:u64` |
| `2` | `REMOVE_SSTABLE` | `sstable_id:u64` |
| `3` | `SET_CHECKPOINT` | `flushed_through_seq:u64, wal_segment_id:u64, wal_offset:u64` |

`SET_CHECKPOINT`'s last two fields are exactly a `WalPosition` (WAL Spec §5) — reused directly, not re-derived, so the Manifest's checkpoint record and the WAL's own position type never drift apart.

### 6.2 Recovery

Identical algorithm to WAL Spec §6.2–6.3, applied to the `MANIFEST` file instead of WAL segments: walk frames in order, a torn trailing frame is truncated and discarded (expected, not alarming), any invalid frame that is *not* the trailing one is corruption and escalates the whole engine to `DEGRADED` (Architecture Spec §3.1) rather than being silently skipped. Replaying all valid edits in order reconstructs the current live SSTable set (every `ADD` not yet `REMOVE`d) and the last `SET_CHECKPOINT`.

### 6.3 Non-goals

The Manifest file itself is not compacted/snapshotted in v1 — in a long-running instance with many flush/compaction cycles it grows unboundedly. This is an accepted Phase 0 limitation, explicitly deferred rather than silently ignored: Phase 1's Metadata Manager (Architecture Spec §6) is the component that eventually replaces this mechanism with proper durable, compact metadata storage.

---

## 7. Recovery

This is the procedure that makes Architecture Spec §8.3 concrete for the full Phase 0 engine (extending, not replacing, the WAL-only recovery already specified in WAL Spec §6.3 and §8.3).

### 7.1 Sequence

```
1. Replay MANIFEST (§6.2) → live SSTable set + last SET_CHECKPOINT
   (if MANIFEST replay reports corrupted_segments-equivalent: escalate to DEGRADED, stop here)
2. Sweep sstables/ directory (§7.2)
3. Open every SSTable in the reconstructed live set (§2.8's open()); a footer/index
   checksum failure on any of them escalates the engine to DEGRADED (Phase 0 has no
   partition-level granularity yet to degrade just one key range — see §7.4)
4. Wal::open_for_recovery(...) (WAL Spec §6) → all valid WAL records + truncates any torn tail
5. Discard WAL records with seq <= checkpoint.flushed_through_seq (already reflected
   in the live SSTable set); replay the remainder, in order, into a fresh empty MemTable
   via MemTable::insert (§1.2) — this fresh memtable becomes the new `active`
6. immutables starts empty (§7.3 explains why this is always correct)
7. Resume service
```

### 7.2 SSTable directory sweep

Delete every `*.sst.tmp` file unconditionally (Section 3.3 — always an interrupted build). For every `*.sst` file present on disk but **not** in the Manifest-reconstructed live set: if the Manifest's edit history shows it was `ADD`ed and later `REMOVE`d, delete it (a compaction's old input that a crash left physically undeleted — Section 5.2 step 8 is what normally handles this, but a crash before that step runs leaves it for this sweep instead). If it was never `ADD`ed at all (the crash-between-fsync-and-manifest-write case from Section 3.3), re-append an `ADD_SSTABLE` edit for it instead of deleting it — the file is valid and durable, it just hadn't been acknowledged yet.

### 7.3 Why a crashed flush never needs special-case recovery

If a crash occurs mid-flush (between Section 4.4 steps 1–4), the frozen memtable that was being flushed is never in the Manifest's live set (its `ADD_SSTABLE` edit, if any, only happens after the SSTable file is fully durable — Section 3.2), and its `.sst.tmp` is swept away per Section 7.2. But nothing is lost: every record that memtable held is still in the WAL (it was written there before ever reaching the memtable, per Section 4.3), and since no `SET_CHECKPOINT` was ever written for that flush, those records still have `seq > checkpoint.flushed_through_seq` and get replayed by step 5 above, right back into a fresh active memtable. This is why `immutables` always starts empty after recovery — anything that was ever in that list either finished flushing (and is now safely in the live SSTable set, excluded from replay by the checkpoint) or never finished (and is fully reconstructed from the WAL instead). There is no third case.

### 7.4 DEGRADED conditions (Phase 0 scope)

Because Phase 0 has no partitions yet, any unresolvable recovery ambiguity degrades the *entire* engine, not a key range within it (Architecture Spec §3.1's partition-level `DEGRADED` granularity is a Phase 1+ refinement). Concretely: Manifest corruption that isn't a clean trailing torn write; a live-set SSTable whose footer or index checksum fails; or the defensive invariant check in step 5 (if WAL replay is non-empty, its first record's `seq` should equal `checkpoint.flushed_through_seq + 1` — see WAL Spec §10's retention guarantee for why this should always hold by construction; a violation means something impossible happened and must not be silently patched over).

### 7.5 Test checklist

1. Clean shutdown and restart: all data present, `next_seq`/`next_sstable_id` correctly resumed.
2. Crash after WAL `append_sync` but before memtable `insert` — record recovered via WAL replay.
3. Crash mid-flush (`.sst.tmp` present, no Manifest `ADD_SSTABLE`) — sweep removes the temp file, WAL replay reconstructs the data, no duplication and no loss.
4. Crash after SSTable fully durable but before its `ADD_SSTABLE` Manifest edit — sweep re-adds it; data is not re-replayed from WAL for records already covered by *that* SSTable (verify no double-application, since the checkpoint for it wasn't written yet — this is the one genuine edge case worth a dedicated test: confirm the read path correctly finds the value in the now-recognized SSTable rather than relying on a stale WAL replay that may or may not still include it depending on purge timing).
5. Crash mid-compaction between `ADD_SSTABLE`(new) and one or more `REMOVE_SSTABLE`(old) edits — restart, verify correct (possibly redundant, never wrong) data.
6. Corrupted (non-tail) Manifest — engine reports `DEGRADED`, does not silently drop data or guess.
7. Corrupted SSTable footer on a live-set file — engine reports `DEGRADED`.
8. Full crash-injection fuzz test: randomized operation sequences (puts, deletes, flush triggers, compaction triggers) with a randomized crash point across ≥1,000 runs; assert the post-recovery state is always exactly explainable as "some prefix of durable operations," never a gap, never a duplication, never silent corruption acceptance.

---

## 8. Performance Targets

As with the WAL spec, no number here is asserted without a benchmark backing it (Architecture Spec §11.2 / WAL Spec §12's shared principle). `benches/lsm_bench.rs` should measure, at minimum: point-lookup latency (p50/p99) as a function of SSTable count and total data size (this is precisely what the bloom filter and block index exist to bound); sequential write throughput; range-scan throughput at various scan widths; and flush/compaction duration as a function of memtable/SSTable size. These numbers feed directly into the Architecture Spec's cost model calibration (§11.2) once the router phase begins.

---

## 9. Implementation Checklist

- [ ] Memtable: `(key, seq)`-ordered map, `get_as_of` via range+`next_back` (§1)
- [ ] SSTable: block/bloom/index/footer format exactly as specified, byte-for-byte (§2)
- [ ] Atomic SSTable construction: tmp file → fsync → rename → directory fsync → Manifest (§3)
- [ ] LSM facade implementing the full Storage Engine Contract, `ReadView` read pattern (§4)
- [ ] Compaction: size-tiered full-merge trigger, tombstone-safety rule against `snapshot_refs` (§5)
- [ ] Manifest: WAL-frame-format reuse, `ADD`/`REMOVE`/`SET_CHECKPOINT` edits (§6)
- [ ] Full recovery sequence including the directory sweep and the "no special-case flush recovery" property (§7) — this is the piece most likely to be under-tested; do not skip the crash-injection fuzz test (§7.5, item 8)
- [ ] All test-checklist items across §1.6, §2.9, §4.5, §5.3, §7.5 passing
- [ ] `lsm_bench.rs` producing the numbers listed in §8

Once this checklist and the WAL Specification's checklist are both complete, Phase 0 is done: a correct, tested, benchmarked, single-instance LSM storage engine, ready for Phase 1 (metadata + versioning at the partition level) per Architecture Spec §1.7's build order.
