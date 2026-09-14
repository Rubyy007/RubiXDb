# RubixDB WAL Specification v1.0

**Status:** Final — ready for implementation
**Component:** Write-Ahead Log (Phase 0, Step 1)
**Implements:** RubixDB Architecture Specification v1.0, Section 8 (Write-Ahead Log and Recovery Contract)

---

## 0. Purpose and Relationship to the Architecture Spec

This document is the concrete, byte-level specification of the WAL component described abstractly in Section 8 of the RubixDB Architecture Specification v1.0. It is the first real implementation artifact of the project, per the Phase 0 build order: WAL → Memtable → SSTable → LSM facade → Compaction → Recovery → Benchmark.

Everything below is meant to be implemented against directly — a correct implementation is one that passes every test in Section 11 and produces the exact byte layout in Section 2.

### 0.1 Deliberate Phase 0 simplifications, and why they don't cost a format break later

The Architecture Spec's `WalRecord` (Section 8.1) carries `seq`, `partition_id`, `op`, `key`, `value`, and `crc`. Phase 0 has exactly one storage engine and no partitions yet, so `partition_id` is meaningless at this stage. Rather than omit it and pay for a format migration in Phase 1, this spec reserves its place structurally: the segment header carries a `format_version`, and Section 2.5 defines exactly how `format_version = 2` adds `partition_id` without invalidating anything written under `format_version = 1`. Similarly, `op = ENGINE_SWITCH` (needed only in Phase 5, for migration) is reserved as a defined-but-unused enum value now, so its wire format doesn't need to be invented later under time pressure.

This is the general principle: **Phase 0 implements a subset of behavior, never a subset of format.** The bytes on disk are designed for the whole project; only the code paths that use them grow over time.

### 0.2 Non-goals for this component

Encryption at rest is out of scope here — it belongs to the Storage Manager / File Manager layer (Architecture Spec Section 16) and, if enabled, wraps the WAL's I/O transparently below this spec, not inside it. Compression is out of scope for v1. A sharded or multi-writer WAL is out of scope for v1 (Section 8 defines the single-writer model; see Section 9).

---

## 1. Design Goals

- **Durability:** once `sync()` returns `Ok`, every record appended before it is guaranteed to survive a power loss (assuming the underlying disk honors `fsync`).
- **Torn-write detection, not torn-write prevention:** a crash mid-append is expected and normal. Recovery must always detect exactly where the log becomes unreliable and never read past that point.
- **Corruption must never be confused with a torn write.** A torn write is possible only at the physical tail of the log at the moment of a crash. A checksum failure anywhere else (an earlier, previously-fsynced record) is real corruption — bit rot, a disk error, a software bug — and must be escalated loudly, not silently truncated away. Conflating the two is the single most dangerous mistake a WAL implementation can make: it turns real data loss into a recovery that "succeeds."
- **Sequential-only.** The WAL is append-only. No in-place mutation of a previously-written record, ever.
- **Format stability under growth.** See Section 0.1 — the byte format anticipates the fields Phase 1 and Phase 5 will need.

---

## 2. On-Disk Format

### 2.1 Directory layout

```
<data_dir>/wal/
  wal-00000000000000000001.log
  wal-00000000000000000002.log
  wal-00000000000000000003.log      ← active (tail) segment
```

Segment files are named `wal-{segment_id:020}.log`, `segment_id` a zero-padded `u64` (20 decimal digits, covering the full `u64` range) so lexicographic directory listing equals numeric/creation order. Segment IDs are assigned by the WAL itself, starting at 1, strictly increasing, never reused.

### 2.2 Segment header (24 bytes, once per file, at offset 0)

| Field | Type | Bytes | Value |
|---|---|---|---|
| `magic` | `[u8; 8]` | 8 | ASCII `"RBXWALv1"` |
| `format_version` | `u32 LE` | 4 | 1 for this spec (see Section 2.5 for evolution) |
| `segment_id` | `u64 LE` | 8 | Must equal the ID encoded in the filename; cross-checked on open |
| `flags` | `u32 LE` | 4 | Reserved, must be 0 in v1 |

Total: 8 + 4 + 8 + 4 = 24 bytes. A file shorter than 24 bytes, or with a mismatched magic, is not a valid WAL segment and `open()` fails with `EngineError::Corruption` rather than being silently skipped.

### 2.3 Record frame

Every record after the header is a self-describing, length-prefixed, checksummed frame:

```
+------------------+------------------+---------------------------+
| length : u32 LE  | crc32c : u32 LE  | body : [u8; length]        |
+------------------+------------------+---------------------------+
        4 bytes            4 bytes              `length` bytes
```

`body` is:

```
body := seq(8 bytes, u64 LE) || op(1 byte) || op_body(variable)
length := 9 + len(op_body)
crc32c := CRC32C(body)          // computed over seq || op || op_body, i.e. the exact `length` bytes
```

Frame header is a fixed 8 bytes (`length` + `crc32c`); everything after it, for exactly `length` bytes, is `body`. Using CRC32C (Castagnoli) specifically, not the IEEE/CRC32 variant — it has hardware acceleration on both x86 (SSE4.2) and ARM, and is what every widely-deployed LSM engine's WAL uses for the same reason. The `crc32c` crate is the recommended implementation.

### 2.4 `op` values and `op_body` encodings

| `op` | Name | `op_body` encoding | Used in |
|---|---|---|---|
| 1 | `PUT` | `key_len:u32 LE, key:[u8;key_len], val_len:u32 LE, val:[u8;val_len]` | Phase 0+ |
| 2 | `DELETE` | `key_len:u32 LE, key:[u8;key_len]` | Phase 0+ |
| 3 | `CHECKPOINT_MARKER` | `flushed_through_seq:u64 LE` | Phase 0+ (Section 8) |
| 4 | `ENGINE_SWITCH` | Defined in Architecture Spec §8.1; not emitted before Phase 5 | Reserved now |
| 5–255 | — | Reserved | — |

`CHECKPOINT_MARKER` is written by the engine (not by a client put/delete call) whenever a memtable flush durably completes; `flushed_through_seq` records the highest `seq` now safely represented in an on-disk SSTable and drives the segment-retention rule in Section 10.

### 2.5 Format evolution (why Phase 1 doesn't break this)

When Phase 1 introduces multiple partitions, a new `format_version = 2` is defined with `partition_id: u64 LE` inserted immediately after `seq` in `body` (so `length` becomes `17 + len(op_body)`). Readers are required to dispatch on the `format_version` field of each segment's header, so a WAL directory containing a mix of `format_version = 1` segments (written before the upgrade) and `format_version = 2` segments (written after) is a legal, permanently-supported state — old segments are read correctly forever; they simply have no `partition_id`, which is correct, since Phase 0 has exactly one implicit partition. No migration tool, no rewrite pass, ever needed for this upgrade.

### 2.6 Size limits

`MAX_RECORD_LEN` (config, default 64 MiB = 67,108,864) bounds `length`. On read, if the declared `length` exceeds `MAX_RECORD_LEN`, this is treated as corruption immediately — the reader must not attempt to allocate or read that many bytes on the strength of an unvalidated field, since a corrupted length field is exactly the kind of bit-flip this limit exists to catch cheaply.

---

## 3. Sequence Number Assignment

The WAL is the sole authority for `seq` (this is the concrete implementation of the Architecture Spec's global LSN, Section 7.1, scoped to Phase 0's single engine). `seq` is assigned synchronously, in memory, at the moment `append()` is called — not at `sync()` time — so that append-call order and `seq` order are identical by construction. Because assignment happens inside `append()` and the WAL is single-writer (Section 9), no external locking is needed to keep this monotonic; `append()` itself is the serialization point.

`seq` starts at 1 for a brand-new WAL and is recovered as `max(seq) + 1` (i.e., resumed, not reset) whenever an existing WAL directory is opened — recovery (Section 6) always yields the correct next value from the last valid record.

---

## 4. Durability Policy

`append()` writes bytes into the OS page cache and returns immediately; it makes no durability promise by itself. `sync()` issues `fsync` on the segment file and only then may the caller consider every record appended before that call durable. This is the durability boundary from Architecture Spec Section 8.2, and it is the only place that boundary exists — nothing upstream is allowed to acknowledge a write to its own caller before `sync()` has returned `Ok`.

Two sync modes, selected via `WalConfig::sync_mode`:

- **`Immediate`** (default, and the only mode required for Phase 0): every `append_sync()` call does `append()` then `sync()` before returning. Simple, correct, and the mode all of Section 11's tests are written against.
- **`GroupCommit { max_wait, max_batch_bytes }`** (optional, post-Phase-0): a background flusher batches appends from multiple concurrent callers and issues one `fsync` per batch, waking every caller in that batch once it completes. This requires the WAL to become internally thread-safe (an append queue + one flusher task), which Phase 0's single-writer LSM engine does not need. **Do not implement this before `Immediate` mode is fully tested** — it is explicitly a later-phase optimization, consistent with the project's "don't skip ahead" build order.

---

## 5. API

```rust
pub struct WalConfig {
    pub max_record_len: usize,      // default 64 MiB
    pub max_segment_size: u64,      // default 64 MiB, triggers rotation
    pub sync_mode: SyncMode,        // default SyncMode::Immediate
}

pub enum SyncMode {
    Immediate,
    GroupCommit { max_wait: Duration, max_batch_bytes: usize }, // post-Phase-0
}

pub struct WalPosition {
    pub segment_id: u64,
    pub offset: u64,   // byte offset of this frame's start within the segment
    pub seq: u64,
}

pub enum WalOp<'a> {
    Put { key: &'a [u8], value: &'a [u8] },
    Delete { key: &'a [u8] },
    CheckpointMarker { flushed_through_seq: u64 },
}

pub trait Wal: Sized {
    /// Opens (creating if absent) the WAL directory, recovering and truncating
    /// any torn tail as part of open. This is the normal startup path.
    fn open_for_recovery(dir: &Path, config: WalConfig) -> Result<(Self, WalReplayResult)>;

    /// Appends one record, assigning it the next `seq`. Does NOT fsync.
    fn append(&mut self, op: WalOp) -> Result<WalPosition>;

    /// Fsyncs the active segment; every record appended before this call
    /// (by position) becomes durable once this returns Ok.
    fn sync(&mut self) -> Result<()>;

    /// append() + sync() — the only durability boundary callers should use
    /// unless they are deliberately batching (see Section 4, GroupCommit).
    fn append_sync(&mut self, op: WalOp) -> Result<WalPosition> {
        let pos = self.append(op)?;
        self.sync()?;
        Ok(pos)
    }

    /// Forces rotation to a new segment regardless of current size.
    fn rotate(&mut self) -> Result<()>;

    /// Deletes segments that are entirely older than `watermark_seq`
    /// (see Section 10 for the exact safety condition). Returns the
    /// segment_ids actually removed.
    fn purge_before(&mut self, watermark_seq: u64) -> Result<Vec<u64>>;

    fn current_segment_id(&self) -> u64;
    fn next_seq(&self) -> u64;
}

/// Read-only inspection of a WAL directory without mutating it — used by
/// recovery tooling and tests, never by the normal startup path.
pub fn inspect(dir: &Path, config: &WalConfig) -> Result<WalReplayResult>;

pub struct WalReplayResult {
    pub records: Vec<(u64 /* seq */, WalOpOwned)>, // in seq order
    pub last_valid_position: WalPosition,
    pub truncated: bool,              // a torn tail was found and (for open_for_recovery) discarded
    pub corrupted_segments: Vec<u64>, // non-tail corruption found; requires operator attention
}
```

`open_for_recovery` is the only path that mutates a segment file on the strength of recovery (physically truncating a torn tail — see Section 6.4). `inspect` never writes; it is what a repair tool or a test harness uses to examine a WAL directory without side effects.

If `corrupted_segments` is non-empty after `open_for_recovery`, the caller (the engine, and above it the Partition Manager) must not proceed to `ACTIVE` — this is the Phase-0-scoped instance of the Architecture Spec's rule that unresolvable recovery ambiguity lands a partition in `DEGRADED` (Section 8.3 of the Architecture Spec), not a guess.

---

## 6. Recovery Procedure

### 6.1 Segment enumeration

List all files in `<data_dir>/wal/` matching `wal-*.log`, parse `segment_id` from each filename, sort ascending. This is the replay order — segment IDs are assigned monotonically at creation time (Section 2.1), so filename order is always chronological order.

### 6.2 Per-segment validation

For each segment in order:

1. Open the file, read the 24-byte header (Section 2.2). If the file is shorter than 24 bytes, or `magic` doesn't match, or `segment_id` in the header disagrees with the filename — this segment is corrupt. It is added to `corrupted_segments` and, critically, replay does **not** stop here if it is not the last segment; subsequent segments (which may be entirely intact) are still processed, because the goal is to surface the exact extent of damage, not to treat one bad segment as if everything after it is unreadable too. (If it is the last segment, see 6.3 — an unreadable header on the last segment with zero valid records is still corruption, not a torn write, because a torn write only ever damages the frame currently being appended, never the segment header itself, which is written once and fsynced before any records.)
2. Walk frames sequentially from offset 24: read 8 bytes (frame header: `length`, `crc32c`). If fewer than 8 bytes remain, this is a clean, expected end of segment — stop walking this segment, no error.
3. If `length > MAX_RECORD_LEN`, treat as corruption (Section 2.6) at this offset — see step 5's tail/non-tail distinction.
4. If fewer than `length` bytes remain in the file after the frame header, this is a **torn write**: the writer was interrupted after writing `length`/`crc32c` but before finishing `body`. Stop walking this segment here.
5. Compute `CRC32C(body)` and compare to the stored `crc32c`. On mismatch: if this frame is at the current physical end of all WAL data being scanned (i.e., last segment, and no further complete frames follow) — treat as a torn write (the fsync boundary can leave a partially-flushed page in some filesystem/hardware configurations even at frame granularity). If a checksum mismatch occurs anywhere **not** at that trailing position — i.e., there is at least one more structurally valid frame after it in the log — this is corruption, not a torn write, and the segment is added to `corrupted_segments`. This is the rule from Design Goal 3, made mechanical.
6. On success, decode `body` into `seq`, `op`, `op_body` per Section 2.4 and continue to the next frame.

### 6.3 Determining "torn" vs "corrupt" precisely

A torn write can only ever be the very last incomplete-or-invalid frame in the very last segment being replayed. Any invalid frame that is **not** in that position — a bad frame in an earlier segment, or a bad frame followed by more valid frames even within the last segment — is corruption. This single rule is what Section 6.2 steps 4 and 5 implement, and it is **the load-bearing correctness property of this entire spec**: it is what stops disk bit-rot from being silently discarded as if it were a normal crash artifact.

### 6.4 Truncation

For `open_for_recovery` only (never for `inspect`): once the walk reaches a torn write at the tail, the active segment file is physically truncated to `last_valid_position` (end of the last fully valid frame) before the WAL is handed back to the caller for new appends. This guarantees a subsequent crash-and-recover cycle sees a clean file rather than re-discovering the same torn bytes. `truncated: true` is reported in `WalReplayResult` regardless, so tests and operators can observe that this happened even though it's the expected, non-alarming case.

### 6.5 Output

`records` contains every valid, decoded record across all valid segments, in `seq` order (which, by Section 3, is also append order and file order — no sorting is actually required, only concatenation in segment order). `next_seq()` on the returned `Wal` is `max(seq in records) + 1`, or `1` if `records` is empty.

---

## 7. Segment Rotation

A new segment is created (current segment sealed, new one opened with `segment_id + 1`) when either: the active segment's size reaches `max_segment_size` (default 64 MiB), or `rotate()` is called explicitly (the engine calls this, for example, immediately before writing a `CHECKPOINT_MARKER` for a completed flush, so that the marker and the rotation boundary line up cleanly for the retention rule in Section 10). Rotation itself is: fsync the current segment, close it, create and fsync-open the new segment's 24-byte header, then resume appending. Rotation never straddles a record — a record is always wholly contained in one segment.

---

## 8. Error Handling

All fallible operations return `Result<_, EngineError>` using the shared error enum from Architecture Spec Section 4.2 (`IoError`, `Corruption`, `CapacityExceeded`, etc.). Specifically: an `fsync` failure from `sync()` returns `IoError` and — critically — does not advance any durability watermark the caller may be tracking; the caller must treat every record since the last successful `sync()` as not-yet-durable and retry or surface the failure upward rather than assuming partial progress. A `length > MAX_RECORD_LEN` violation on `append()` (caller-side, before it ever hits disk) returns `CapacityExceeded` synchronously, before any bytes are written, so it never produces a partial on-disk frame in the first place.

---

## 9. Concurrency Model

The WAL is single-writer in v1: `append`/`append_sync`/`rotate`/`purge_before` are `&mut self` and are not internally synchronized. Exactly one owner (the LSM engine's single write path) calls them, serialized by construction rather than by a lock inside the WAL. This matches Phase 0's single-writer LSM engine and avoids building concurrency machinery the current phase doesn't need (per the project's incremental build philosophy). `inspect()` is read-only and safe to call concurrently with an active writer from a different process only if that process opens the file in a read-only, non-exclusive mode — concurrent access from within the same process should simply call `WalReplayResult`-returning methods against the same `Wal` handle's already-buffered state instead.

If and when Phase 0's engine grows multiple concurrent write callers (not currently planned), the `GroupCommit` mode described in Section 4 is the extension point — it is designed so that adding it later does not change the on-disk format, only the internal scheduling of append/sync calls.

---

## 10. Segment Retention

A segment is eligible for deletion by `purge_before(watermark_seq)` only when every record it contains has `seq < watermark_seq` and `watermark_seq` itself has been established as safe by the engine — specifically, `watermark_seq` must be a value the engine obtained from a `CHECKPOINT_MARKER` it itself durably wrote (Section 2.4) and, in turn, that checkpoint must correspond to data now durably present in a validated SSTable (i.e., registered in the Phase 0 engine-local recovery manifest, not merely flushed to an OS page cache). The WAL itself does not know whether an SSTable is valid — it only enforces the arithmetic condition (`seq < watermark_seq`) on the value it's given; the engine is responsible for only ever passing a `watermark_seq` it has proven durable. `purge_before` never deletes the currently-active segment, regardless of the watermark.

---

## 11. Test and Acceptance Checklist

Every item below must pass before this component is considered done. Where "crash" appears, it means a deterministic fault-injection point (a `FaultInjectingIo` wrapper around the file handle that can be told "fail/truncate output after N bytes written" so tests are reproducible), not an actual OS-level process kill — real kill-and-restart tests are valuable too but belong in the integration/crash-test suite one level up (Phase 0's `crash_recovery_tests.rs`, exercising the full LSM engine, not just the WAL in isolation).

1. **Round trip:** append N records (varying op types, including empty-value PUTs and max-length keys/values near `MAX_RECORD_LEN`), sync, close, `open_for_recovery` — all N records returned, in order, with correct `seq` values, `truncated == false`, `corrupted_segments` empty.
2. **Torn write — mid frame-header:** truncate output after 1–7 bytes of a frame's 8-byte header. Recovery returns all prior records, `truncated == true`, no corruption flagged.
3. **Torn write — mid body:** truncate output after the 8-byte frame header but before all `length` body bytes are written, at several offsets (1 byte in, half of `length`, `length - 1` bytes). Same expected outcome as #2.
4. **Torn write exactly at a frame boundary:** kill immediately after a complete frame (simulating a crash between two `append` calls). Recovery returns all complete records, `truncated == false` (nothing was actually torn), `corrupted_segments` empty.
5. **Non-tail corruption is never truncated away:** write 3 valid records, flip a bit inside record 2's `body` on disk, leave record 3 intact and valid. Recovery must return `corrupted_segments` containing this segment and must **not** silently return only record 1 as if record 2 were a normal torn tail — assert the distinction from Section 6.3 explicitly.
6. **Multi-segment replay:** force rotation across 5 segments, verify replay reconstructs the full, correctly-ordered record set from all 5 in filename order.
7. **Torn tail only ever in the last segment:** with 3 segments where only the last is torn, replay returns all records from segments 1–2 intact plus the valid prefix of segment 3, `truncated == true`, no corruption.
8. **Corrupted header on a non-last segment:** replay still processes later, intact segments and reports the corrupted one in `corrupted_segments` (Section 6.2 step 1's "don't stop" rule).
9. **`MAX_RECORD_LEN` enforcement:** `append()` with a payload whose encoded length exceeds the configured max returns `CapacityExceeded` and writes zero bytes (verify via file size unchanged).
10. **`purge_before` safety:** segments containing any record with `seq >= watermark_seq` are never removed, even a single such record; the active segment is never removed regardless of watermark.
11. **`fsync` failure does not advance durability:** inject an `fsync` error; verify the caller-visible durable watermark does not move and a retried `sync()` after the injected failure is cleared succeeds and correctly covers the previously-unsynced records.
12. **`inspect` is side-effect-free:** run `inspect` against a directory containing a torn tail; verify the on-disk bytes are byte-for-byte unchanged afterward (unlike `open_for_recovery`, which truncates).
13. **Sequence resumption:** `open_for_recovery` an existing WAL with records up to `seq = K`, append a new record, verify it receives `seq = K + 1`, not a reset to 1.
14. **Fuzz/property test:** generate a random sequence of append/rotate calls with a random crash point injected at a random byte offset across ≥ 1,000 randomized runs; assert the invariant that replay always returns some prefix of the originally-appended records (never a suffix, never a gap, never out-of-order `seq` values, never a record not among those appended).
15. **Empty WAL:** `open_for_recovery` on a freshly created, empty directory succeeds, returns zero records, `next_seq() == 1`.

---

## 12. Performance Targets

No hard numbers are asserted here — the design principle from the Architecture Spec (Section 1.7 / 11.2) applies equally at this layer: cost coefficients and performance claims are calibrated from measurement, not invented. `benches/wal_bench.rs` should measure, at minimum: `append_sync` latency distribution (p50/p99) under `Immediate` mode at several payload sizes, and achieved throughput in bytes/sec, to establish the baseline every later engine-level benchmark (Architecture Spec Section 17) builds on. These numbers, once measured, become the calibration input for `WriteCost` in the Architecture Spec's cost model (Section 11.2) — this is the concrete link between "write a fast WAL" and "the router makes good decisions later."

---

## 13. Implementation Checklist

- [ ] Segment header read/write (Section 2.2)
- [ ] Frame encode/decode with CRC32C (Section 2.3–2.4)
- [ ] `append` / `sync` / `append_sync` (`Immediate` mode only)
- [ ] `rotate`, size-triggered and explicit
- [ ] `open_for_recovery` with truncate-on-torn-tail (Section 6.4)
- [ ] `inspect` (read-only variant)
- [ ] Tail-vs-non-tail corruption distinction (Section 6.3) — this is the one piece of logic that must not be simplified away
- [ ] `purge_before` with the checkpoint-watermark safety condition (Section 10)
- [ ] `FaultInjectingIo` test harness
- [ ] All 15 tests in Section 11 passing
- [ ] `wal_bench.rs` producing p50/p99 append latency and throughput numbers

Once this checklist is complete, the WAL is ready to be consumed by the Memtable and SSTable components (Phase 0, Steps 2–3), and its byte format does not change again until Phase 1's `format_version = 2` (Section 2.5).
