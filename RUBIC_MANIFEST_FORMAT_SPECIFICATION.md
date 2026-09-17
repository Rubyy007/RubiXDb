# RUBIC Manifest Format Specification (Phase 5)

Status: **Final for Phase 5 — ready for implementation.**

Governance: derived directly and exclusively from
`RubixDB-LSM-Engine-Specification-v1.0.md` §3 ("Atomic SSTable
Construction"), §6 ("Manifest"), and §7 ("Recovery") — all three marked
"Status: Final." Per the specification hierarchy (operating brief §5 of
the Phase 4B brief, still binding), this document quotes and derives
from that already-final spec; it does not re-decide anything the spec
already pins, and it marks anything the spec leaves open as
**UNDEFINED — ASK REQUIRED** rather than guessing.

---

## 0. Is the Manifest a "RUBIC-family" file?

**Explicitly not decided by this document, and not silently assumed.**
`RUBIC_FORMAT_SPECIFICATION.md` mentions "a future manifest-like
structure" exactly once (§3.8, in passing, discussing metadata
versioning in the abstract) and never formally brings the Manifest
under its governance policy (magic + `format_version` header, §3.1) the
way it explicitly did for the WAL (§1, "not yet a formally adopted
name") and RUBIC SSTable (§2). The Manifest's own component-level
specification (LSM Engine Spec §6.1) defines its file as a flat,
headerless sequence of frames starting at byte 0 — no file-level magic,
no file-level `format_version`. Per the specification hierarchy, the
component-level spec governs the byte-level detail; this document
follows it exactly: **the Manifest file has no magic bytes and no
file-level format-version field.** Whether the Manifest is later
formally named "RUBIC Manifest" and brought under the family's
magic/version discipline is a separate, deliberate decision for a
future phase, mirroring the WAL's own precedent — not decided here,
not silently inherited.

---

## 1. File identity, location, and directory layout

```
<data_dir>/MANIFEST
```

A single, append-only file, one per engine instance, at the top level
of the data directory — the same directory the WAL's own segment files
and `LOCK` file already occupy, and the parent of the `sstables/`
subdirectory Phase 4B introduced. No Manifest rotation, no multiple
Manifest files, no Manifest-selection ambiguity this phase (Section 9).

---

## 2. Record (frame) format — reused, not reinvented

Per LSM Engine Spec §6.1: "Reuses the WAL's exact frame format (WAL
Spec §2.3)":

```
Frame :=
  length   : u32 LE      // byte length of body
  crc32c   : u32 LE      // CRC32C over body
  body     : [u8; length]
```

**Implementation note (flagged, not silently done):** `wal::format::
encode_frame` cannot be called directly for this — its current
implementation always prepends an 8-byte `seq` before the op tag inside
`body` (the WAL's own record-body layout, `seq(8) || op(1) || op_body`),
but the Manifest's body layout (Section 3 below) has no `seq` prefix at
all. Refactoring `encode_frame` to decouple that baked-in `seq` field
was judged higher-risk (touching already-certified, 200+-test-covered
WAL code) than reimplementing this ~15-line, trivially-testable
`length || crc32c || body` wrapper independently in
`src/manifest/format.rs` — see `PHASE5_ADR.md`. The two are
byte-compatible at the frame-header level (verified by a dedicated
cross-check unit test), so "reuses the WAL's exact frame format" is
honored at the specified byte level without sharing Rust code across
the module boundary. `wal::format::encode_frame`'s own doc comment has
been corrected to no longer claim direct reusability it doesn't
actually provide.

`crc32c` — the same `crc32c` crate this project already depends on
(WAL, SSTable). No new dependency.

---

## 3. Edit types — exactly the three the spec defines, no more

Per LSM Engine Spec §6.1, `body := edit_type(1 byte) || type_fields`:

| `edit_type` | Name | `type_fields` | Encoded size (bytes, excluding `edit_type`) |
|---|---|---|---|
| `1` | `ADD_SSTABLE` | `sstable_id:u64 LE, min_seq:u64 LE, max_seq:u64 LE, file_size:u64 LE` | 32 |
| `2` | `REMOVE_SSTABLE` | `sstable_id:u64 LE` | 8 |
| `3` | `SET_CHECKPOINT` | `flushed_through_seq:u64 LE, wal_segment_id:u64 LE, wal_offset:u64 LE` | 24 |

No other edit type is defined by the authoritative specification, and
none is invented here. In particular: **no per-SSTable key-range
field**, **no level field** (Phase 0/5 has no leveled compaction — only
the flat, size-tiered, full-merge strategy LSM Engine Spec §5 defines,
itself out of scope this phase), and **no generic "extension" edit
type**. All three type-fields sets are fixed-width, little-endian,
matching this project's project-wide integer-encoding convention
(`RUBIC_FORMAT_SPECIFICATION.md` §3.5).

`SET_CHECKPOINT`'s `wal_segment_id`/`wal_offset` are exactly the
`segment_id`/`offset` fields of the `WalPosition` (`src/wal/mod.rs`)
returned when the engine durably appends the corresponding
`WalOp::CheckpointMarker` record to the WAL (Section 6 below) — **not**
a new position type, and **not** `WalPosition::seq` (that field is
deliberately not reused here: the checkpoint marker's own sequence
number, freshly assigned by `GroupCommitter::append` when the marker
itself is written, is necessarily *larger* than `flushed_through_seq`,
which is the max `seq` already covered by the just-published SSTable —
conflating the two would be a real bug, not a simplification). This
`wal_segment_id`/`wal_offset` pair is recorded for traceability/tooling
(Section 12's inspection tool) but is **not** required by the recovery
algorithm's own correctness (Section 8) — recovery uses
`flushed_through_seq` alone to decide what to discard.

### 3.1 Encoding

```
encode_add_sstable(id, min_seq, max_seq, file_size) -> body:
  body[0] = 1
  body[1..9]   = id.to_le_bytes()
  body[9..17]  = min_seq.to_le_bytes()
  body[17..25] = max_seq.to_le_bytes()
  body[25..33] = file_size.to_le_bytes()
  // total body length: 33

encode_remove_sstable(id) -> body:
  body[0] = 2
  body[1..9] = id.to_le_bytes()
  // total body length: 9

encode_set_checkpoint(flushed_through_seq, wal_segment_id, wal_offset) -> body:
  body[0] = 3
  body[1..9]   = flushed_through_seq.to_le_bytes()
  body[9..17]  = wal_segment_id.to_le_bytes()
  body[17..25] = wal_offset.to_le_bytes()
  // total body length: 25
```

Each `body` above is then wrapped by the Section 2 frame
(`length || crc32c || body`) before being appended to the file.

### 3.2 Decoding and validation

A decoded edit is validated **before** it is trusted to update
in-memory recovery state (operating brief: "The reader must validate
every record before trusting metadata"):

- `edit_type` not in `{1, 2, 3}`: corruption (`EngineError::Corruption`,
  detail names the found byte and file offset).
- Body length not exactly matching the `edit_type`'s fixed size (33, 9,
  or 25 respectively): corruption — a length mismatch can only mean a
  bad frame, never a valid record of a different shape (there is no
  variable-length field in any edit type).
- `ADD_SSTABLE` with `min_seq > max_seq`: corruption — an SSTable's own
  footer already enforces `min_seq <= max_seq` (`RUBIC_SSTABLE_FORMAT_
  SPECIFICATION.md` §2.7), so a Manifest edit claiming otherwise
  describes an impossible SSTable and must never be trusted.
- `REMOVE_SSTABLE` for an `sstable_id` never `ADD`ed by any
  earlier-replayed edit: corruption — "invalid SSTable reference," per
  operating brief's explicit corruption-class list. (A `REMOVE_SSTABLE`
  for an id that *was* added and already removed — a duplicate/replayed
  edit — is not corruption; see Section 7's idempotence rule.)
- `SET_CHECKPOINT` with `flushed_through_seq` less than the
  **currently-tracked** checkpoint (the highest `flushed_through_seq`
  from any `SET_CHECKPOINT` edit replayed so far in this same pass):
  corruption — "impossible checkpoint regression," per operating
  brief's explicit corruption-class list. The checkpoint is a monotonic
  durable boundary (Section 5); a persisted record claiming it moved
  backward describes something that cannot happen under this project's
  own write-path invariants and must not be silently accepted or
  silently clamped.

---

## 4. Recovery algorithm — identical to the WAL's own torn-vs-corrupt rule

Per LSM Engine Spec §6.2: "Identical algorithm to WAL Spec §6.2–6.3,
applied to the `MANIFEST` file instead of WAL segments."

Concretely, walking the file from byte 0:

1. Read the next frame's 8-byte header (`length`, `crc32c`). If fewer
   than 8 bytes remain: **torn tail** (the writer was interrupted after
   a complete prior frame but before starting a new one, or mid-header)
   — stop walking, this is expected, not alarming, exactly the WAL's
   own Spec §6.2 step 4 rule applied here.
2. If fewer than `length` bytes remain after the frame header: **torn
   tail** (interrupted mid-body) — stop walking.
3. Compute `CRC32C(body)` and compare to the stored `crc32c`. On
   mismatch: if this frame is at the current physical end of the file
   being scanned (no further complete frame follows) — **torn tail**
   (a torn `fsync` boundary can leave a partially-flushed page even at
   frame granularity, identical reasoning to WAL Spec §6.2 step 5). If
   a checksum mismatch occurs anywhere **not** at that trailing
   position — i.e., at least one more structurally valid frame follows
   it — this is **corruption**, not a torn write, and escalates
   immediately (Section 4.1) rather than being skipped.
4. Decode the body per Section 3.2's rules. A validation failure here
   (unknown `edit_type`, wrong length, impossible field value) is
   **corruption**, regardless of position — Section 3.2's checks are
   never "expected, tail-only" failures the way a torn write is; they
   indicate a structurally well-formed-looking frame with logically
   invalid content, which can only mean a software bug or bit rot, per
   `RUBIC_FORMAT_SPECIFICATION.md` §3.11's shared torn-vs-corruption
   principle.
5. Apply the valid, decoded edit to the in-memory recovery state
   (Section 5) and advance to the next frame.

### 4.1 Corruption escalation

**Any non-tail corruption escalates `LsmEngine::open` to a hard `Err`**
— per LSM Engine Spec §6.2 ("escalates the whole engine to `DEGRADED`")
and §7.4's own note that Phase 0 has no partition-level granularity to
degrade just one key range. Continuing this project's own established
Phase 4A/4B precedent (`PHASE4B_ADR.md` ADR-P4B-2): since this
single-instance engine has no partition lifecycle state machine at all
(that is an Architecture Spec §3.1 concept for the future, partitioned
Phase 1+ engine — genuinely absent from this codebase, not merely
unexposed), "the engine reports `DEGRADED`" is realized concretely, for
Phase 5, as **`LsmEngine::open` returning `Err`, refusing to start** —
not a runtime-queryable degraded status distinct from a failed open.
This is not a new decision; it is the same realization Phase 4B already
applied to SSTable corruption, applied consistently to Manifest
corruption.

### 4.2 Torn-tail handling

A torn trailing frame is **truncated and discarded**, exactly like a
torn WAL segment tail — "expected, not alarming" (LSM Engine Spec
§6.2). Unlike the WAL's `open_for_recovery` (which physically truncates
the file), the Manifest's read-then-append lifecycle this phase truncts
**in memory only** during the recovery pass (the file itself is
physically truncated to the last valid frame boundary the first time
the engine successfully appends past that point, which naturally
overwrites/extends past the torn bytes — see Section 6's append
contract). This mirrors `wal::inspect`'s own "never mutate on read"
posture for the read-only recovery pass (Section 10's locking
discussion), deferring physical truncation to the point where a real
write already needs to happen there anyway.

---

## 5. Checkpoint semantics — the central requirement of this phase

**A checkpoint is a monotonic durable boundary `flushed_through_seq`
such that every record with `seq <= flushed_through_seq` is guaranteed
to be represented in the Manifest-authoritative live SSTable set.** It
is not merely "the highest `seq` some SSTable's footer happens to
claim" (`PHASE4B_ADR.md` ADR-P4B-1 already rejected exactly that
footer-derived heuristic as a hidden Manifest reimplementation) — it is
**only** ever established by a durably-recorded `SET_CHECKPOINT` edit,
which itself is only ever written after:

1. The corresponding SSTable is fully durable on disk (temp file ->
   fsync -> rename -> directory fsync, `RUBIC_SSTABLE_FORMAT_
   SPECIFICATION.md` §3.4, unchanged).
2. Its `ADD_SSTABLE` edit has been durably appended to the Manifest and
   fsynced (LSM Engine Spec §3.2 step 5).
3. A `CHECKPOINT_MARKER` WAL record for the same `flushed_through_seq`
   has been durably appended to the WAL (WAL Spec §2.4/§10 — see
   Section 6 below).

Only then may `SET_CHECKPOINT{flushed_through_seq, wal_segment_id,
wal_offset}` itself be appended and fsynced.

**Monotonicity is enforced at two independent layers, deliberately
redundant:** (a) the in-memory recovery/runtime state never accepts a
`SET_CHECKPOINT` whose value is less than the currently-tracked one
(Section 3.2 — a regression is corruption, not silently clamped or
ignored); (b) the engine's own write path (Section 6) never
*constructs* a regressing `SET_CHECKPOINT` in the first place, because
`flushed_through_seq` for a given flush is always the just-frozen
memtable's own `max_seq`, and memtables are frozen in strictly
increasing sequence order by construction (the active memtable's
sequence range never overlaps a previously-frozen one, MemTable's own
`(user_key, seq)` global-uniqueness invariant). Layer (a) exists
specifically to catch the case layer (b)'s own invariant somehow
failed to prevent — belt and suspenders, not redundant guessing.

---

## 6. WAL `CHECKPOINT_MARKER` — an already-defined, previously-inert op, now wired up

`WalOp::CheckpointMarker { flushed_through_seq: u64 }` (`src/wal/
ops.rs`) and its `OP_CHECKPOINT_MARKER = 3` byte already exist,
encode/decode correctly, and have since Phase 4A — Phase 4A's own
`apply_wal_op` treated it as a pure no-op during replay ("there is no
flush-to-SSTable/checkpoint concept yet," `PHASE4A_MEMTABLE_
ARCHITECTURE.md` §10's own closing note). Phase 5 does not change the
WAL binary format at all for this — it makes durable, meaningful use of
an op the format has supported since Phase 4A.

Per WAL Spec §2.2's segment-rotation note: **the engine calls
`rotate()` immediately before writing a `CHECKPOINT_MARKER`**, "so the
marker and the rotation boundary line up cleanly for the retention rule
in Section 10." This is not optional cleanup — WAL Spec §10 states the
`purge_before(watermark_seq)` precondition explicitly: `watermark_seq`
"must be a value the engine obtained from a `CHECKPOINT_MARKER` it
itself durably wrote... and, in turn, that checkpoint must correspond
to data now durably present in a validated SSTable (i.e., registered in
the ... manifest)." `purge_before` itself only enforces the arithmetic
condition (`seq < watermark_seq` for every record in a candidate
segment) — the engine is solely responsible for only ever calling it
with a `watermark_seq` that has actually satisfied this whole chain.

The `CHECKPOINT_MARKER` is written via the **same** durability path as
any other write — `BatchCoordinatorPool::submit(WalOpOwned::
CheckpointMarker{flushed_through_seq})` followed by `Completion::
wait()` — reusing the existing leader/follower group-commit machinery
exactly, with **no new sequence system** (its `seq` is assigned by the
same `GroupCommitter::append` every other record uses).

---

## 7. Idempotent recovery

Per operating brief: "Replaying the same Manifest record sequence twice
must produce the same logical storage state." This holds by
construction from the edit semantics already defined:

- Replaying `ADD_SSTABLE(id, ...)` for an `id` already in the live set
  (an edit somehow durably written twice — cannot happen via this
  phase's own single-flush-thread write path, but recovery does not
  assume that; LSM Engine Spec §3.3 explicitly calls a redundant
  `ADD_SSTABLE` "safe and idempotent — it just describes a file that's
  already correctly on disk") is a no-op: the live set is a set, not a
  multiset, keyed by `sstable_id`.
- Replaying `REMOVE_SSTABLE(id)` for an `id` already removed (not
  currently live, but *was* live at some earlier point in this same
  replay) is a no-op — removing an absent element from a set is a
  no-op. Only `REMOVE_SSTABLE` for an id **never** `ADD`ed at all
  (Section 3.2) is corruption; "already removed" is a legitimate,
  tolerated redundancy, not an error.
- Replaying `SET_CHECKPOINT` with the **same** value as the
  currently-tracked checkpoint is a no-op (not a regression, per
  Section 5's `<` — not `<=` — comparison). Only a strictly *smaller*
  value is corruption.

No test needs to *construct* a scenario producing duplicate edits from
this phase's own write path (there is no compaction, and flush-retry
never re-appends a manifest edit for an attempt that already
succeeded, Section 11) — but the recovery algorithm tolerates them
regardless, since the specification's own idempotence language applies
to whatever the file legitimately contains, not only to what this
phase's own writer happens to produce.

---

## 8. Live-state reconstruction (bounded memory)

Recovery reconstructs an in-memory `ManifestState`:

```rust
struct ManifestState {
    live_sstables: BTreeMap<u64, SstableManifestEntry>, // keyed by sstable_id
    ever_added: HashSet<u64>,     // every id ever ADD_SSTABLE'd, live or not --
                                   // needed to distinguish "already removed"
                                   // (idempotent, Section 7) from "never added"
                                   // (corruption, Section 3.2)
    checkpoint: Option<CheckpointState>, // None until the first SET_CHECKPOINT
}

struct SstableManifestEntry {
    min_seq: u64,
    max_seq: u64,
    file_size: u64,
}

struct CheckpointState {
    flushed_through_seq: u64,
    wal_segment_id: u64,
    wal_offset: u64,
}
```

Built by **sequential replay, one frame at a time** — the Manifest file
is never read into one giant in-memory buffer/`Vec` of all historical
edits (operating brief: "bounded-memory during Manifest recovery...
prefer sequential replay into the live-state representation"). Memory
usage is proportional to the **current live SSTable count** (bounded by
however many SSTables actually exist — itself naturally bounded, since
each one is a real file on disk) plus `ever_added` (bounded by the
total number of SSTables ever created across this engine's lifetime —
this does grow unboundedly over a very long-running instance with no
compaction/Manifest-compaction, an accepted, explicitly-named
consequence of LSM Engine Spec §6.3's own "not compacted in v1" limit,
carried forward unchanged, not newly introduced by this phase), never
proportional to the number of *edits* in the file. This is the direct
Manifest analogue of `wal::replay_streaming`'s own bounded-memory
design (`PHASE4A_MEMTABLE_ARCHITECTURE.md` §10) — reusing that same
principle, not a new one.

---

## 9. Manifest discovery and rotation

**No rotation, no compaction, no multiple Manifest files this phase** —
LSM Engine Spec §6.3 explicitly: "The Manifest file itself is not
compacted/snapshotted in v1... an accepted Phase 0 limitation,
explicitly deferred rather than silently ignored." There is exactly one
authoritative file, `<data_dir>/MANIFEST`; if it does not exist, this is
a fresh engine (empty live set, no checkpoint — equivalent to
`flushed_through_seq = 0`, replay the entire WAL, matching Phase 4A's
existing "no prior state" behavior exactly). No "latest file wins"
heuristic is needed or implemented, because there is never more than
one candidate file to choose among.

---

## 10. Locking / concurrency model

The Manifest is protected by the **same** exclusive/shared lock file
(`LOCK`, `src/wal/file_io.rs`) the WAL already uses for the entire data
directory — not a second, Manifest-specific lock file. That lock's own
documented intent ("only one writer may have a WAL directory open at a
time") already covers the whole directory's durable state, not merely
WAL segment files narrowly; `sstable::discover`'s own Phase 4B
directory sweep already relied on this same trust boundary without a
dedicated lock of its own, and the Manifest continues that precedent.

`acquire_shared_lock_if_present`/`acquire_exclusive_lock` (`src/wal/
file_io.rs`) are re-exported `pub(crate)` (mirroring `fsync_dir`'s own
Phase 4B precedent, `PHASE4B_ADR.md` ADR-P4B-4) so `src/manifest` can
reuse them directly rather than duplicating platform lock-acquisition
logic a second time.

See `PHASE5_MANIFEST_ARCHITECTURE.md` §4 for the full `LsmEngine::open`
startup-ordering derivation this requires (a genuine, non-obvious
interaction with the existing `replay_streaming`-before-
`FileWal::open_for_recovery` lock-ordering constraint from
`PHASE4A_ADR.md` ADR-P4A-3).

Once open, only the single background flush thread appends to the
Manifest (no compaction, no second writer this phase) — no additional
in-process synchronization beyond ordinary Rust ownership is needed for
writes; observability reads (Section 12) read the already-shared,
already-`Arc<RwLock<...>>`-protected in-memory `ManifestState`/
`sstables` list, never the file directly, on any hot path.

---

## 11. What remains UNDEFINED / deferred (not guessed)

- Manifest compaction/snapshotting (LSM Engine Spec §6.3's own explicit
  v1 non-goal) — deferred to whatever future phase introduces it, not
  attempted here.
- Per-SSTable key-range tracking in `ADD_SSTABLE` — not part of the
  authoritative spec's field list; not added speculatively.
- Level tracking — not applicable; no leveled compaction exists or is
  implemented.
- A magic/`format_version` header for the Manifest file — deliberately
  left undecided (Section 0), matching the WAL's own naming/governance
  precedent.
- Any Manifest-level extension mechanism for a future edit type beyond
  the three defined here — not needed until a real motivating use case
  exists, per this project's own "calibrate from measurement/real need"
  principle.
