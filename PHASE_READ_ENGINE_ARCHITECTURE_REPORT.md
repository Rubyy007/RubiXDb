# RubiXDB Read Engine — Architecture Report (read-only audit, no implementation)

**Status:** Phase 1 deliverable only — grounded architecture report. No source code was modified to produce this document. Compaction, Router, and Replication were not touched or started.

**Scope note, stated up front and load-bearing for everything below:** `RubixDB-Architecture-Specification-v1.0.md` describes a full multi-engine, partitioned, adaptive-routing system (Log/LSM/B+Tree engines, Partition Manager, Metadata Manager, Cost Model, Adaptive Router, Migration Manager, Query Layer — its §3–§19). **None of that exists in this codebase.** What exists is exactly one engine — the LSM engine — implementing `RubixDB-LSM-Engine-Specification-v1.0.md` (Steps 2–6) on top of the already-certified WAL (`RubixDB-WAL-Specification-v1.0.md`, Step 1). This report is scoped entirely to that single, already-implemented, non-partitioned LSM engine. Anywhere the Architecture Spec's language (`point_lookup`, `range_scan`, `snapshot()`, `EngineError`) is used below, it is the LSM Engine Spec's own realization of that contract (§4 of the LSM spec), not the multi-engine abstraction layer, which is genuinely absent.

---

## 1. Existing read-related code — inventory

| File | What it contains (read-relevant) |
|---|---|
| `src/lsm/mod.rs` | `LsmEngine::get`/`get_as_of` (lines 671, 685) — the facade-level point-lookup read path. `snapshot_seq()` (line 716) — a bare `u64`, not a handle type. No `range_scan`, no `contains`, no `batch_get`, no `snapshot()` returning a `SnapshotHandle`. |
| `src/sstable/reader.rs` | `SsTable::open` (full validation, bounded memory), `SsTable::get_versioned` (point lookup within one file), `SsTable::range_scan_raw` (**already implemented and tested** — ordered, bounded-memory, per-block iteration over `[start, end)`). |
| `src/sstable/bloom.rs` | `BloomFilter::might_contain` (line 101) — the negative-lookup fast path `get_versioned` calls first. |
| `src/sstable/format.rs` | `decode_block`, `decode_index_block`, `decode_bloom_block`, `Footer::decode` — the on-disk-to-in-memory decode functions `open()`/`read_block()` call. |
| `src/sstable/mod.rs` | `discover()` — directory sweep helper; per `PHASE5_MANIFEST_ARCHITECTURE.md` §2 its role narrowed to "a validation/sweep helper the Manifest-driven startup path calls," no longer the liveness authority. |
| `src/memtable/mod.rs` | `MemTable::get`/`get_as_of` (lines 134, 146) — `range((key,0)..=(key,as_of_seq)).next_back()`, exactly as `RubixDB-LSM-Engine-Specification-v1.0.md` §1.2 specifies. `MemTable::range` (line 174) — raw ordered iteration, already implemented. |
| `src/manifest/mod.rs` | `replay_readonly`, `open_after_exclusive_lock`, `record_count`, `last_edit`, `size_bytes` — all either startup/observability, none of them a runtime read-path dependency (confirmed: `ManifestState`, the structure with the live-SSTable set, is local to `LsmEngine::open()` and is never retained — see §5 below). |
| `src/error.rs` | `EngineError`: `NotFound`, `Corruption{detail}`, `Io(io::Error)`, `WalUnavailable{detail}`, `Unsupported{operation}`, `CapacityExceeded{requested,max}`, `Aborted{detail}`, `InvalidPath{detail,path}`, `Timeout{detail}`, `StorageExhausted{detail}` (added 2026-09-19, write-path only). Every variant a read call can currently surface: `NotFound` (not actually returned by `get`/`get_as_of` today — see §7), `Corruption`, `Io`, `Unsupported` (unrecognized SSTable `format_version`), `CapacityExceeded` (oversized `key_len`/`value_len` read from disk). |
| `examples/sstable_bench.rs` | **Already measures** SSTable point-lookup latency (p50/p95/p99, warm), bloom-filter false-positive rate, and reader RSS delta — at the single-open-`SsTable` layer, not through `LsmEngine`. |
| `examples/memtable_bench.rs` | Already measures `MemTable::get_value` latency. |

**Nothing here is a stub.** `get`/`get_as_of` is a real, working, tested read path today — described precisely in §5–§6 below, not proposed.

---

## 2. Existing SSTable reader capabilities (from source)

- `SsTable::open()` (`src/sstable/reader.rs:73-140`): opens the file, validates footer magic/version/checksum, validates bloom and index blocks (checksum + `bloom_end == index_offset` contiguity + `index_end == usable_len` exactness), and eagerly loads only the footer + Bloom filter + index into memory. **Data blocks are never read at `open()`** — confirmed by the function body, not just its doc comment.
- `get_versioned(key, as_of_seq)` (`reader.rs:181-226`): (1) `bloom.might_contain(key)` — a definite miss returns `Ok(None)` with zero file I/O; (2) `index.partition_point(|e| e.last_key < key)` — **not** `binary_search_by`, deliberately, because a key whose version run spans a block boundary (consecutive blocks sharing an identical `last_key`) requires the *leftmost* matching index, which `binary_search_by` does not guarantee on ties (`reader.rs:188-196` comment); (3) extends `candidates` forward while consecutive blocks share that same `last_key` (`reader.rs:202-206`) — this is a real extension beyond the LSM Engine Spec §2.8's literal algorithm text (which describes only "the one block"), documented in-source as handling the boundary case the format spec's own §2.6 discussion and test-checklist item 7 anticipate; (4) reads and checksum-verifies each candidate block via `read_block`, linear-scans for the exact key, tracks the highest `seq <= as_of_seq`.
- `range_scan_raw(start, end)` (`reader.rs:233-256`, iterator at `reader.rs:259-323`): already implemented, ordered, bounded-memory (one block materialized at a time via `current: std::vec::IntoIter<DecodedRecord>`), stops and yields exactly one `Err` on first corrupted block, never continues past it.
- Positional reads (`read_exact_at`, `reader.rs:24-52`): no shared file cursor, safe for concurrent callers sharing one `Arc<SsTable>` with no lock — platform-specific (`pread`-style on Unix, `seek_read` loop on Windows).
- Corruption classification (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §6, table at lines 516-528): every `.sst`-extension corruption is real corruption, **never** a torn write (unlike WAL/Manifest segments) — because an SSTable only ever bears its final name after being made fully durable via the atomic-rename discipline (§3). Footer/index/bloom corruption fails `open()` closed; data-block corruption is detected **lazily**, at `get_versioned`/iteration time, returned as `Err(Corruption)` for that one access — confirmed by `read_block`'s call site inside `get_versioned`/`RangeScanRaw::next`, never inside `open()`.
- Tests already covering this: `reader.rs:337-414` (`point_lookup_across_versions`, `range_scan_matches_memtable_content`, `range_scan_respects_bounds`, `open_rejects_truncated_file`).

---

## 3. Existing Manifest capabilities (from source)

- `Manifest` struct (`src/manifest/mod.rs:63-69`): `{ file: File, record_count: u64, last_edit: Option<ManifestEdit> }` — small, retained for the engine's lifetime, **not** the live-SSTable-set structure.
- `ManifestState` (`src/manifest/state.rs:29-36`): `{ live_sstables: BTreeMap<u64, SstableManifestEntry>, ever_added: HashSet<u64>, checkpoint: Option<CheckpointState> }` — this is the potentially-large, replay-derived structure. Confirmed by `grep` (this session, during the memory investigation): used **only** as a local variable inside `LsmEngine::open()` (`src/lsm/mod.rs`, passed `&mut` into `reconcile_sstables_with_manifest`), dropped when `open()` returns. **Not retained anywhere for runtime reads.** A Read Engine addition must not reintroduce a live, retained copy of this structure (see §13).
- Read-relevant public API: `replay_readonly(dir)` (startup, shared lock), `open_after_exclusive_lock(dir)` (startup, exclusive lock), `record_count()`/`last_edit()`/`size_bytes()` (observability only, lock the `Manifest` mutex, read the small retained struct — never the file, never `ManifestState`).
- Manifest is the **sole SSTable-liveness authority** (`PHASE5_MANIFEST_ARCHITECTURE.md` §1, §6): `LsmEngine.sstables` is populated exclusively from Manifest-reconstructed, footer-validated tables at `open()` and by the flush thread's own publish step — never by "every `.sst` file found in the directory." An orphaned `.sst` not in the live set is never served.

---

## 4. Existing MemTable capabilities (from source)

- `MemTable::get(key)` / `get_as_of(key, as_of_seq)` (`src/memtable/mod.rs:134-158`): `range((key,0)..=(key,as_of_seq)).next_back()` over a `BTreeMap<(Vec<u8>, u64), MemtableValue>` — exactly `RubixDB-LSM-Engine-Specification-v1.0.md` §1.2's specified pattern, byte-for-byte matching implementation.
- `MemTable::range(start, end)` (`memtable/mod.rs:174`): raw, all-versions, `(key asc, seq asc)` iteration — already implemented; version resolution/tombstone filtering is explicitly the caller's job (spec §1.2), not the MemTable's.
- Tombstones: `MemtableValue::Tombstone` is a first-class map value, never physically removed by the MemTable itself (spec §1.4 — GC only happens in Compaction, which does not exist).
- Concurrency: `MemTable` is not internally synchronized (spec §1.5, single-writer principle); a frozen `MemTable` (`freeze()` → `Arc<MemTable>`) is safe to share across threads because the type system removes every `&mut` path once frozen — confirmed as a real, not merely documented, Rust ownership guarantee.

---

## 5. Existing LsmEngine capabilities (from source) — the current read path is real, not a stub

`LsmEngine::get_as_of` (`src/lsm/mod.rs:685-716`, `get` at 671 delegates to it with `as_of_seq = u64::MAX`):

```
{ active read-lock }      -> active.get_as_of(key, as_of_seq); return if found
{ immutables read-lock }  -> for imm in immutables.iter() (newest-first, VecDeque::push_front on freeze):
                                imm.get_as_of(key, as_of_seq); return on first hit
{ sstables read-lock }    -> for table in sstables.iter() (newest-first, list.insert(0, ...) on publish):
                                table.get_versioned(key, as_of_seq)?; return on first hit
                             (propagates SsTable I/O errors — Corruption, Io — as Err)
Ok(None) if nothing found
```

This **already is** the recency-ordered, first-hit-wins merge `RubixDB-LSM-Engine-Specification-v1.0.md` §4.2 describes for `get()`. `resolve()`/`resolve_sstable()` collapse `Tombstone` → `None`, `Put(v)` → `Some(v)`, applied once at the very end — matching spec §4.2's explicit "collapse happens once."

**What a Read Engine phase would add or hardern, not build from scratch:**
- `range_scan` at the `LsmEngine` level does not exist. `MemTable::range` and `SsTable::range_scan_raw` both already exist and are individually tested; nothing currently k-way-merges them across sources with version resolution.
- `snapshot()` returning a real handle type does not exist — only `snapshot_seq() -> u64` (a bare watermark, not an object that can be held to keep tombstones/old versions alive against a future Compaction's `snapshot_refs` check, LSM Engine Spec §4.2). Since Compaction does not exist, nothing currently needs `snapshot_refs` to be honored — but the *type* gap (no `SnapshotHandle`) is real and would need deciding before Compaction can be built correctly later, even though it is out of scope to build now.
- `contains(key)` does not exist as a distinct method (today: callers use `get(key).is_some()`, which does strictly more work — a full value read — than a bloom+index-only existence check would need).
- The concurrency pattern is **not** exactly the spec's described `ReadView` (see §8 — this is a real, precise divergence, not a nitpick).

---

## 6. Proposed Read Engine flow vs. what's actually implemented

The brief's assumed flow —

```
GET(key) -> mutable MemTable -> Immutable MemTables -> Manifest live SSTables
          -> newest-to-oldest SSTable lookup -> tombstone/version resolution
          -> final value / NotFound
```

— **is exactly what `get_as_of` already does today**, verified against source in §5, with two precise refinements worth stating explicitly rather than leaving implicit:
1. "Manifest live SSTables" is not consulted directly on the read hot path — `LsmEngine.sstables` is the already-reconciled, Manifest-authoritative, footer-validated `Vec<Arc<SsTable>>`; the Manifest's own file/struct is never touched per read (confirmed §3). The brief's flow is correct at the conceptual/authority level, not as a literal per-read call sequence.
2. "final value / NotFound" — the current `get`/`get_as_of` never actually returns `EngineError::NotFound`; a miss is `Ok(None)`, not `Err(NotFound)`. `NotFound` exists in `EngineError` (§1's inventory) but nothing in the read path constructs it. This is a genuine, previously-unflagged discrepancy between the error enum's stated intent and current behavior — see §7 and §14.

---

## 7. API contract — what exists vs. what a Read Engine phase should add

| Method | Exists today? | Notes |
|---|---|---|
| `get(key) -> Result<GetResult>` (`GetResult = Option<Vec<u8>>`) | **Yes** (`lsm/mod.rs:671`) | Miss is `Ok(None)`, not `Err(NotFound)` — see §14, open question. |
| `get_as_of(key, as_of_seq) -> Result<GetResult>` | **Yes** (`lsm/mod.rs:685`) | Snapshot point-lookup, already the LSM Engine Spec §4.1's `point_lookup`. |
| `range_scan(start, end, as_of_seq) -> Iterator<...>` | **No** | Building blocks (`MemTable::range`, `SsTable::range_scan_raw`) exist and are tested individually; the `LsmEngine`-level k-way merge across sources with version resolution + tombstone collapse (spec §4.2's `range_scan` paragraph) does not exist. |
| `contains(key) -> Result<bool>` | **No** | Not in either spec's required-operations table (Architecture Spec §4.1 doesn't list it either) — an optional convenience, not a contract requirement. Only worth adding if it can be meaningfully cheaper than `get(key).is_some()` (bloom+index short-circuit without reading the value) — needs a design decision, not an assumption. |
| `batch_get(keys) -> Result<Vec<GetResult>>` | **No** | Not required by either spec. If added, should be defined as "no cross-key atomicity, just batched dispatch" to avoid inventing a consistency guarantee the specs don't promise. |
| `snapshot() -> SnapshotHandle` | **No** (only `snapshot_seq() -> u64`) | Real gap vs. LSM Engine Spec §4.1's `snapshot()`/§4.2's snapshot registration — but note Compaction (the only consumer of `snapshot_refs`) is explicitly out of scope for this phase too, so building a full handle type now has no caller that needs it yet. Flagged as an open question (§14), not assumed to be in scope. |

**Errors**: `get`/`get_as_of` today can return `Err(Corruption)` (propagated from `SsTable::get_versioned`/`open`) or `Err(Io)`. Do not invent new error variants without checking `EngineError` first (§1) — every corruption/I/O shape the read path can hit already has a home in the existing enum.

---

## 8. Concurrency model — precise divergence from the spec's `ReadView` pattern, traced not assumed

`RubixDB-LSM-Engine-Specification-v1.0.md` §4.2 describes capturing **one** combined `ReadView { active, immutables, sstables }` (clone all three `Arc` sets, release all locks, then do the entire merge against that frozen snapshot).

**The actual implementation does not do this.** `get_as_of` (`lsm/mod.rs:685-716`) takes and releases three **separate, sequential** read-lock scopes — `{ lock_active_read(); ... }`, then `{ lock_immutables_read(); ... }`, then `{ lock_sstables_read(); ... }` — returning early from whichever scope finds a hit, never holding more than one lock at a time, and never capturing a single unified snapshot.

**Whether this divergence is safe was traced, not assumed**, against the actual flush-thread publish ordering (`PHASE5_MANIFEST_ARCHITECTURE.md` §5, and confirmed in `spawn_flush_thread`, `lsm/mod.rs`): the flush thread inserts the newly-flushed `SsTable` into `sstables` (step 4 of that ordering) **before** it removes the corresponding entry from `immutables` (step 9). So during the flush's brief transition window, the just-flushed data is visible in **both** `immutables` and `sstables` simultaneously — never in neither. Since `get_as_of` checks `immutables` before `sstables`, a read racing this transition either sees the data in `immutables` (checked first) or, if it arrives slightly later, in `sstables` — either way it finds the correct version, and the "first hit wins" recency rule is preserved because nothing else can produce a *different, wrong* version for the same key during this window (an SSTable's data, once published, cannot be superseded except by a strictly newer write, which would itself sort correctly ahead of it in `active`/`immutables`).

**This is not a proof, it is a traced argument specific to the flush path** — it does not by itself establish that the sequential-lock pattern is safe under every future concurrent mutation this codebase might add (e.g., once Compaction exists and can *remove* SSTables mid-read, not just add them). **Open question, not resolved here**: should a Read Engine phase (a) leave the sequential-lock pattern as-is with this documented reasoning, (b) adopt the spec's literal `ReadView` capture-then-merge pattern for a cleaner, more obviously-correct invariant going forward (at the cost of holding three `Arc`-clone snapshots per read instead of releasing locks progressively), or (c) something else. Not decided in this report — flagged for Phase 3 (API contract)/an ADR if implementation begins.

**Test gap, precisely identified**: `RubixDB-LSM-Engine-Specification-v1.0.md` §4.5 item 5 — "Concurrent readers during an in-progress flush observe either the fully-pre-flush or fully-post-flush state for any single read — never a torn mix" — is a **required test** per the spec's own checklist. Searched `src/lsm/tests.rs` (30 `#[test]` functions, full list in this session's own audit): no test with this exact shape exists today. The closest is `flush_moves_data_into_a_published_sstable_that_remains_readable`, which checks readability *after* a flush completes, not concurrent-with-in-flight-flush read consistency. **This is a real, concrete, currently-missing test**, not a hypothetical — see §16.

**A reader can never observe a partially-published SSTable**: `SsTable::open()` only ever runs against a file already fully durable under its final name (atomic rename discipline, LSM Engine Spec §3) — a `.sst.tmp` is never opened as live, and the `sstables` list is only ever populated with the result of a completed `SsTable::open()` call. Structurally guaranteed by the type/API design, not merely by convention.

---

## 9. Consistency/visibility model — already implemented, described precisely

- **Sequence-number rule**: every stored value carries the `seq` it was written at (WAL-assigned, `GroupCommitter::append`); `get_as_of(key, as_of_seq)` returns the highest `seq <= as_of_seq` across every source, matching `RubixDB-Architecture-Specification-v1.0.md` §7.2's version-resolution rule exactly.
- **Newest-wins / recency-ordering**: enforced structurally by iterating `active` → `immutables` (newest-first `VecDeque`) → `sstables` (newest-first `Vec`) and returning the *first* hit — never comparing sequence numbers across sources, because construction already guarantees the first source checked can only hold newer-or-equal data than any source checked after it.
- **Tombstone semantics**: a tombstone is a first-class `MemtableValue`/`RecordValue` variant at every layer (MemTable, SSTable); the public `get`/`get_as_of` collapse it to `None` **once**, at the very end (`resolve`/`resolve_sstable`, `lsm/mod.rs`) — never collapsed early, so an intermediate source's tombstone correctly shadows an older source's `Put` for the same key.
- **Snapshot reads**: `get_as_of(key, as_of_seq)` already provides point-in-time reads at an arbitrary past `seq`. What's missing is a `range_scan` equivalent and a real `snapshot()` handle (§7) — the underlying seq-based mechanism is already correct and tested (`snapshot_reads_remain_stable_across_later_writes`, `lsm/tests.rs`).

---

## 10. Corruption behavior — traced from source, fail-closed today

From `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §6 (table, verified against `reader.rs`'s actual error-construction call sites, not just the spec text):

| Corruption | Detected | Behavior |
|---|---|---|
| Bad footer magic / `footer_crc32c` / file `< FOOTER_SIZE` | `SsTable::open()`, eagerly | `Err(Corruption)` — engine `open()` fails closed for a live-set table (LSM Engine Spec §7.1 step 3) |
| Unrecognized `format_version` | `Footer::decode` | `Err(Unsupported{operation})`, not `Corruption` — a real, deliberate distinction already in the format spec |
| Bad index/bloom checksum, out-of-bounds offset/length, overlapping index blocks | `SsTable::open()`, eagerly | `Err(Corruption)` |
| Bad data-block checksum | `read_block`, called only from `get_versioned`/`RangeScanRaw::next` | `Err(Corruption)` **lazily**, for that one access only — never scanned eagerly at `open()`, confirmed by call-site location |
| Malformed record (`op` not in `{1,2}`, length past block end) | `format::decode_block` | `Err(Corruption)` at decode time |
| Oversized `key_len`/`value_len` before allocation | Format decode, pre-allocation check | `Err(CapacityExceeded{requested,max})` — fail-closed, not "corruption" in the checksum sense, but equally refuses to trust the value |
| Missing live SSTable (Manifest says live, file absent) | `LsmEngine::open()`'s reconciliation sweep | `Err(Corruption)` — already tested, `missing_live_sstable_fails_closed_on_open` |
| Orphan `.sst` not in Manifest's `ever_added` | `LsmEngine::open()`'s sweep | Re-added via a fresh `ADD_SSTABLE` edit (not an error) — already tested, `garbage_orphan_sstable_file_fails_closed_not_silently_handled` (name suggests fail-closed; the actual behavior for this specific case is re-registration per `PHASE5_MANIFEST_ARCHITECTURE.md` §4 step 6a — worth double-checking this specific test's assertions match that nuance if implementation work touches this area) |
| Manifest corruption (non-tail) | `LsmEngine::open()` | `Err(Corruption)` — already tested, `manifest_corruption_fails_closed_on_open` |

**All of this already fails closed.** A Read Engine phase's corruption-handling work is about *extending coverage* (e.g., a dedicated fuzz/property test injecting corruption at every one of the table rows above and asserting the specific error shape, not just "some error"), not building fail-closed behavior from nothing.

---

## 11. Recovery behavior — reads after `LsmEngine::open()`'s already-certified recovery path

Per `PHASE_WRITE_ENGINE_CERTIFICATION.md`'s RECOVERY/CHECKPOINT gates (both PASS) and `PHASE5_MANIFEST_ARCHITECTURE.md` §4's startup sequence: by the time `LsmEngine::open()` returns `Ok`, `sstables` holds exactly the Manifest-authoritative, footer-validated live set, and `active` holds a freshly-reconstructed MemTable from WAL replay of every record with `seq > checkpoint.flushed_through_seq`. A `get`/`get_as_of` call issued immediately after a successful `open()` is reading against this fully-reconciled state — there is no additional "recovery-aware" read logic needed, because recovery's job is to make `sstables`/`active` correct *before* any read is possible, not to have reads themselves reason about recovery state. This has been exercised, not just asserted: every soak and crash-cycle test in the Write Engine certification (205 external crash cycles, 10 storage-pressure crash cycles, both realistic-soak runs) ends with a reopen-and-verify step that is, in effect, a read-after-recovery correctness check, even though none of them are framed as "Read Engine" tests per se.

---

## 12. Performance benchmark plan

**Already measured** (`examples/sstable_bench.rs`, `examples/memtable_bench.rs`): single-`SsTable` point-lookup p50/p95/p99 (warm), bloom-filter false-positive rate, reader RSS delta, `MemTable::get_value` latency.

**Not yet measured, concrete plan for a new `examples/read_engine_bench.rs`** (or an addition to `lsm_flush_load_test.rs`'s pattern), matching this project's existing benchmark-harness conventions (real process, real RSS sampling via `Get-Process`, p50/p95/p99 via a sorted-latencies buffer, no synthetic/mocked I/O):

| Measurement | What it isolates |
|---|---|
| MemTable hit | `LsmEngine::get` for a key only in `active` |
| Immutable MemTable hit | key only in a frozen-but-not-yet-flushed `immutables` entry |
| SSTable hit, 1 table | key only in the sole live SSTable |
| SSTable hit, N tables (N = 10/100/1000+, reusing this session's memory-investigation scaling methodology) | cost of the newest-to-oldest SSTable scan as live count grows |
| SSTable miss (bloom-negative) | should be near-zero-I/O per the design; measure to confirm |
| SSTable miss (bloom-positive, index-negative) | the "wasted block read" cost the spec explicitly accepts (§2.4) |
| Tombstone lookup | same cost path as a `Put` hit at the storage layer, but confirm the public API's `None` result timing matches |
| Repeated lookup (same key) | should show no different cost than a cold one — confirms no accidental caching/memoization either way |
| Cold vs. warm OS page cache | run once immediately after `open()` vs. after a warm-up pass |

Track: p50/p95/p99, throughput (ops/sec), CPU%, RSS (start/min/max — this project's own harness convention as of the 2026-09-20 memory investigation), and count of block/file reads issued (would need a small counter added to `SsTable` or `read_block` — not present today, a genuine new instrumentation need, not a re-use of an existing counter).

**Do not optimize before this baseline exists** — no read-path change (e.g., adding a block cache) should be proposed or built until these numbers are in hand, per the spec's own §8 principle ("no number... asserted without a benchmark backing it") and this project's established rigor convention.

---

## 13. Memory model — carrying forward the ~176-178 KB/SSTable figure, and what must not double it

`PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` established, with two independent R²=0.9999 measurements, that each open `SsTable` already retains its full Bloom filter (`bits: Vec<u8>`, ~125 KB at ~100K records/table in the measured workload) and full sparse index (`index: Vec<IndexEntry>`, ~15 KB on-disk, more in-memory due to per-entry owned-`Vec<u8>` allocator overhead) for the engine's entire lifetime — traced to `SsTable`'s own struct fields (`reader.rs:58-65`), confirmed to be the *only* growing structure anywhere in the write path.

**Direct implication for a Read Engine phase, stated precisely**: this Bloom filter and index are not something a Read Engine layer "adds" — they are already fully loaded and owned by the `SsTable` struct that already exists, one instance per entry in `LsmEngine.sstables`. A Read Engine addition must not:
- Build a second, parallel index or Bloom-filter structure "for reads" that duplicates what `SsTable` already holds (e.g., a read-side key→block cache reconstructing what `index: Vec<IndexEntry>` already is).
- Retain a second reference-counted copy of the live SSTable set beyond the existing `Arc<RwLock<Vec<Arc<SsTable>>>>` (a naive `range_scan` implementation that clones `Vec<Arc<SsTable>>` per call is fine — cloning `Arc`s is cheap, reference-count bumps only — but cloning the *pointed-to* `SsTable` contents, or building a separate "reader-owned" index copy, would be exactly the double-retention this section warns against).
- Add a data-block cache without first measuring (§12) whether the existing bounded-memory, read-on-demand design (data blocks never cached, explicitly by design per `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` and re-confirmed unchanged through Phase 4B/5) is actually a measured bottleneck. `sstable_bench.rs` already has warm point-lookup numbers; a cache's benefit (or lack of one) should be argued from those plus new multi-table numbers (§12), not assumed.

Reader/file-handle/index/Bloom-filter/block-buffer lifetime, precisely: all tied to the `Arc<SsTable>`'s own lifetime (one `File` handle, one `BloomFilter`, one `Vec<IndexEntry>`, no separate reader object with its own lifetime today — there is no `SsTableReader` distinct from `SsTable` in current source). A block buffer (`Vec<DecodedRecord>` from `read_block`) is function-local, freed when `get_versioned`/the range-scan iterator's `current` field is replaced — already bounded, not cached, confirmed by reading `read_block`'s call sites.

---

## 14. Open architectural questions — real gaps, not settled by the specs or current code

1. **`NotFound` vs. `Ok(None)`**: `EngineError::NotFound` exists in the enum but the current `get`/`get_as_of` never constructs it — a miss is `Ok(None)`. The Architecture Spec §4.1 table's `get` signature is `Result<Option<VersionedValue>>` (matching current behavior, `NotFound` unused for this operation), but the same table's error-model section (§4.2) lists `NotFound` as one of the shared `EngineError` variants without saying which operations produce it. **Not resolved by either spec.** A Read Engine phase should decide explicitly (and document) whether `NotFound` is ever meant to be returned by any read operation, or whether it exists in the enum for a different, non-read purpose (e.g., a future `Unified Read Layer`-level "no such partition" case that doesn't exist yet) — do not silently leave this ambiguous once `range_scan`/`contains` are added, since a new method might be tempted to invent inconsistent behavior here.
2. **`ReadView` divergence (§8)**: whether the sequential-per-source-lock pattern should be kept (with its traced-but-not-formally-proven safety argument) or replaced with the spec's literal single-snapshot `ReadView` capture. Needs a decision, ideally backed by the missing concurrent-flush-read test (§8, §16) actually being written first so the decision has evidence behind it either way.
3. **`snapshot()` handle type**: building a real `SnapshotHandle` now has no consumer (Compaction doesn't exist), but designing `range_scan`/`contains` without deciding what a future `snapshot()` will look like risks an API mismatch later. Worth at least sketching the shape (not implementing) before `range_scan` ships, so the two aren't designed independently and then found incompatible.
4. **Whether `range_scan`'s merge belongs in `LsmEngine` or a separate "Read Engine" module**: the LSM Engine Spec places it in "the LSM Engine Facade" (§4), i.e., `LsmEngine` itself — there is no separate "Read Engine" component in either spec. The user's phase name ("Read Engine") should not be read as implying a new top-level module unless a real reason to split it out emerges; default to extending `LsmEngine`, matching the spec.
5. **Block/file-read counters for benchmarking (§12)**: no existing counter distinguishes "answered from bloom filter alone" vs. "one block read" vs. "N candidate blocks read" (the boundary-extension case, §2). Needed for the benchmark plan to actually explain its own numbers, not just report latency — a real, currently-missing piece of instrumentation.
6. **`contains(key)`/`batch_get(keys)` necessity**: neither is required by either spec. Worth confirming there's an actual caller/use case before adding either, rather than adding them because the phase brief mentioned them as possibilities.

---

## 15. Exact files that must change (if/when implementation begins)

| File | Why |
|---|---|
| `src/lsm/mod.rs` | Add `range_scan` (k-way merge across `active`/`immutables`/`sstables`, version resolution, tombstone collapse — mirroring `get_as_of`'s existing merge-order logic); decide and implement the `NotFound` question (§14.1); resolve the `ReadView` question (§14.2) if the decision is to change the locking pattern. |
| `src/sstable/reader.rs` | Only if instrumentation counters (§14.5) are added — a small, additive change (e.g., an `AtomicU64` counter for blocks read), not a rewrite of `get_versioned`/`range_scan_raw`, both already correct and tested. |
| `examples/read_engine_bench.rs` (new) | The benchmark plan in §12 — new file, matching this project's existing `examples/*_bench.rs`/`*_load_test.rs` conventions, not a modification of the write-path benchmarks. |
| `src/lsm/tests.rs` | The missing concurrent-flush-read test (§8, §16) and any new `range_scan`/`contains` correctness tests. |
| A new `PHASE_READ_ENGINE_ADR.md` or similar | If §14.2 (`ReadView`)/§14.3 (`snapshot()` handle) require an actual architectural decision before implementation — matching this project's own established convention (every frozen-write-path-shaped decision gets an ADR, e.g. `ADR-WE-SP-001`) applied here to a frozen-read-path-shaped decision. |

No changes to `src/memtable/`, `src/manifest/`, `src/wal/`, or `src/error.rs` (beyond possibly deciding to leave it alone per §14.1) are indicated by this audit — every read-path building block those modules need to expose already exists and is already correct.

---

## 16. Exact tests that must be added

Cross-referenced against the 30 tests already in `src/lsm/tests.rs` plus the reader/memtable/manifest module tests (§1's inventory) so nothing already covered is proposed again:

1. **Concurrent-flush-read consistency** (`RubixDB-LSM-Engine-Specification-v1.0.md` §4.5 item 5) — confirmed missing in §8. The single highest-priority new test.
2. **`range_scan` correctness**, once implemented: merged, correctly ordered, tombstone-filtered results across active + immutable + multiple SSTables (LSM Engine Spec §4.5 item 7) — currently untestable since the method doesn't exist.
3. **Multi-source version resolution**, generalized beyond what `snapshot_reads_remain_stable_across_later_writes` already covers: same key present across active memtable, an immutable memtable, *and* an SSTable simultaneously at three different `seq`s in one test (LSM Engine Spec §4.5 item 2) — today's tests cover pairs of these, not confirmed to cover all three at once in a single assertion.
4. **Corruption-injection matrix for reads specifically**: one test per row of §10's table, each asserting the *specific* error variant (`Corruption` vs. `Unsupported` vs. `CapacityExceeded`), not just "returns some `Err`." `missing_live_sstable_fails_closed_on_open`/`manifest_corruption_fails_closed_on_open`/`open_rejects_truncated_file` already exist for a few rows; the data-block-checksum-failure-during-a-live-read case (as opposed to at `open()`) is not obviously covered by an existing test name.
5. **`get_as_of` immediately after every one of the 8 crash points in `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`'s/LSM spec §7.5's recovery test checklist** — the existing crash-cycle tests (205 + 10 external cycles) verify *recovery itself* succeeds and data isn't lost; a read-specific test should additionally assert that a `get`/`get_as_of` call issued right after reopening returns the exact expected value for keys written right before each crash point, not just "recovery didn't error."
6. **Bloom-negative / bloom-positive-index-negative cost distinction**, once §14.5's counters exist — not a correctness test, but a benchmark-backed assertion (e.g., "a bloom-negative lookup issues zero block reads") that would catch a regression silently turning the fast path into a slow one.
7. **`NotFound` vs. `Ok(None)` regression test**, once §14.1 is decided — lock in whichever behavior is chosen so a future change doesn't silently flip it.

None of items 1–7 exist today under these exact shapes — verified against the full test list in §1, not assumed absent.
