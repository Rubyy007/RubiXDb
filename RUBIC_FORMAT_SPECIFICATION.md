# RUBIC Storage Format Specification v0.1 (Family Policy)

Status: **Draft — foundation only.** Defines the RUBIC family's shared
conventions and governance. Does **not** define, invent, or amend any
concrete on-disk byte layout — those either already exist (the WAL) or
are already fully specified elsewhere (the future RUBIC SSTable, per
`RubixDB-LSM-Engine-Specification-v1.0.md` §2) and are referenced, not
duplicated, below. Anything genuinely undefined is marked **UNDEFINED —
RESERVED FOR SSTABLE DESIGN** rather than guessed.

## 0. What RUBIC is

**RUBIC is RubiXDB's native, database-specific persistent storage
format family** — not a general-purpose columnar format, and
specifically **not Parquet**. Parquet is a third-party, general-purpose
columnar file format used across many unrelated systems for analytical
workloads; it has no relationship to this project beyond superficial
"binary file format" similarity. RUBIC is purpose-built for this
project's own LSM storage engine: its record model, ordering
guarantees, and recovery contract are shaped by RubiXDB's own
`(user_key, seq, op)` data model (`RubixDB-LSM-Engine-Specification-
v1.0.md` §1.1) and its own crash-consistency requirements — not by any
external format's schema or design goals.

Terminology used consistently across this project from this point
forward:

| Term | Meaning |
|---|---|
| **RUBIC Storage Format** | The family name for every RubiXDB-native on-disk format under this governance policy |
| **RUBIC WAL** | See §1 — **not yet a formally adopted name**; the existing WAL format is not renamed by this document |
| **RUBIC SSTable** | The future persistent, immutable, sorted on-disk table format — layout already specified (§2), not yet implemented (Phase 4B) |
| **RUBIC block** | A fixed-role byte range within a RUBIC-family file (e.g., a data block, the bloom filter block, the index block, the footer — see §2) |
| **RUBIC metadata** | Any RUBIC-family file's own self-describing header/footer/index structure, as opposed to the record payload it describes |

## 1. The WAL is not renamed

**The existing WAL on-disk format (`RubixDB-WAL-Specification-v1.0.md`)
is an already-established compatibility boundary — production data has
already been written in it across Phases 0-3.** This document does
**not** rename it "RUBIC WAL," does not change its magic bytes, frame
layout, segment header, or any other byte-level detail, and does not
imply that adopting the RUBIC name for future formats retroactively
reclassifies the WAL. If this project later decides the WAL should be
formally absorbed into the RUBIC family (sharing this document's
versioning/governance policy explicitly rather than by implication),
that is a **separate, deliberate, versioned specification decision** —
written down, reviewed, and never silent — not an automatic consequence
of this document existing. Until such a decision is made and recorded,
"WAL" and "RUBIC" name two things this project keeps conceptually and
terminologically distinct: the WAL is RubiXDB's durability log; RUBIC
(today) names the future persistent SSTable family and this shared
governance policy layer.

## 2. RUBIC SSTable — layout already specified, not re-specified here

The concrete byte layout for the first RUBIC-family persistent format —
what this document calls **RUBIC SSTable** — is **already fully
specified**, "Status: Final — ready for implementation," in
`RubixDB-LSM-Engine-Specification-v1.0.md` §2 (file layout, data
record, data block, bloom filter block, index block, 72-byte footer)
and §3 (atomic construction via temp-file-then-rename). This document
does not repeat or re-derive that layout — see that spec directly.
Phase 4A does **not** implement any part of it (operating brief §5, §37).

**Existing, already-chosen concrete details from that spec, carried
forward here for traceability, not reinvented:**
- Footer magic: `"RBXSST01"` (8 bytes), `format_version: u32 LE = 1`.
- Endianness: little-endian for every multi-byte integer field, project-
  wide (WAL Spec §2.1's convention, reused identically here — "no
  translation layer exists between" the WAL and the LSM formats, per
  the LSM Engine Spec §0.2).
- Checksum: CRC32C (Castagnoli), via the same `crc32c` crate this
  project's WAL already depends on (no new dependency) — every RUBIC
  block/footer that needs integrity verification uses it, identically
  to the WAL's own frame checksums.
- `op` byte values: `1 = PUT`, `2 = DELETE` — identical to WAL Spec
  §2.4, deliberately, so no translation layer exists anywhere between
  the WAL, the Memtable, and the future RUBIC SSTable (LSM Engine Spec
  §0.2).

**Whether `"RBXSST01"`'s magic bytes are renamed to reflect the RUBIC
brand (e.g., something spelling out "RUBIC") when Phase 4B actually
implements the SSTable is a separate, deliberate decision for that
phase to make explicitly** — not guessed or decided here, and not
silently inherited from this document's own naming. See §1's "do not
silently rename" principle, applied here by the same reasoning even
though no code yet exists for this format: the *specification* is
already final, and changing a spec's already-decided byte-level detail
still deserves the same deliberateness as changing a shipped format's.

## 3. RUBIC family conventions (governance layer — applies to any future RUBIC-family format, including RUBIC SSTable)

### 3.1 Magic / version identification

Every RUBIC-family file identifies itself unambiguously via a fixed-
position magic byte sequence plus an explicit `format_version: u32 LE`
field, checked before any other interpretation of the file — mirroring
the WAL's own `magic`/`format_version` discipline (WAL Spec §2.2) and
the already-specified RUBIC SSTable footer (§2 above). A reader must
reject a file whose magic does not match its expected family/type
outright, never attempt best-effort interpretation of an unrecognized
magic.

### 3.2 File-type identification

A RUBIC-family reader must be able to determine which RUBIC file type
(e.g., RUBIC SSTable vs. any future RUBIC-family type) it has opened
from the file's own bytes alone, without relying on the filename or
directory location as the source of truth — the magic bytes (§3.1) are
that identification; filenames/extensions are a convenience, not a
correctness mechanism.

### 3.3 Format versioning policy

- `format_version` is a flat `u32`, incremented for any change to a
  RUBIC-family type's on-disk byte layout that an old reader could not
  correctly interpret.
- A reader encountering a `format_version` it does not recognize
  **must fail closed** (report a clear, typed error) rather than guess
  at partial compatibility — matching the WAL's own `decode_segment_
  header`'s existing behavior (rejects an unrecognized `format_version`
  outright, `ARCHITECTURE.md`'s "WAL hardening pass" section).
- Introducing a new `format_version` is always a **separately
  documented, versioned specification decision** (operating brief
  §29) — never a silent byte-layout change under an unchanged version
  number.

### 3.4 Endianness

Little-endian, for every multi-byte integer field in every RUBIC-family
format, without exception — the same project-wide convention the WAL
already established (WAL Spec §2.1) and the RUBIC SSTable spec already
uses (§2 above).

### 3.5 Integer encoding rules

- Fixed-width integers (`u32 LE`, `u64 LE`) for every length-prefix,
  count, offset, and checksum field — no variable-length integer
  encoding (no varint/LEB128) anywhere in the RUBIC family, matching
  the WAL's own fixed-width discipline (WAL Spec §2.3/§2.4) and the
  RUBIC SSTable spec's own fields (§2 above).
- Every length/count field that sizes a subsequent read must be
  validated against a documented maximum **before** being used to
  allocate or read — mirroring the WAL's `max_record_len` enforcement
  (WAL Spec §2.6) — and all arithmetic on such a field uses checked or
  saturating operations, never raw `+`/`*`, per this project's
  standing Non-Negotiable security rule.

### 3.6 Checksum strategy

CRC32C (Castagnoli), applied per logical unit (a WAL frame; a RUBIC
SSTable data block, bloom filter block, index block, and footer —
each independently checksummed, not one checksum over the whole file)
so a single corrupted region is detectable and classifiable without
invalidating unrelated regions of the same file. No new checksum
algorithm or crate is introduced by this document.

### 3.7 Record / block identity

A RUBIC-family record is identified by the same `(user_key, seq)`
ordering pair the Memtable already uses (`RubixDB-LSM-Engine-
Specification-v1.0.md` §1.1) — no RUBIC-family format invents a
different identity scheme for the same logical data. A RUBIC block
(§0's terminology) is identified by its position within its containing
file (offset + length, as the already-specified SSTable index block
already records, §2 above) plus, where applicable, its own checksum
trailer.

### 3.8 Metadata versioning

Any RUBIC-family file's self-describing metadata (a footer, an index,
a future manifest-like structure) carries its own `format_version`
(§3.3) independent of the record payload's own versioning — a future
change to how the index is laid out, for example, need not imply a
change to how data records themselves are encoded, and vice versa.
**UNDEFINED — RESERVED FOR SSTABLE DESIGN**: the exact mechanism for
a RUBIC SSTable to carry *multiple independently-versioned* metadata
sections simultaneously (as opposed to one file-level `format_version`
covering everything, which is what the already-specified footer in §2
above actually does) is not decided by this document and is not
needed until a real forward-compatibility scenario requires it.

### 3.9 Forward-compatibility policy

A RUBIC-family reader from an *older* build encountering a file written
by a *newer* build:
- If `format_version` is unrecognized: fail closed (§3.3) — no
  best-effort partial read.
- **UNDEFINED — RESERVED FOR SSTABLE DESIGN**: whether a future RUBIC
  SSTable format reserves explicit per-field "feature flag" bits (so a
  minor, additive change could in principle be read by an older
  reader that simply ignores flagged-but-unrecognized fields) is not
  decided here. The already-specified footer (§2 above) has no such
  flags field today; adding one is itself a `format_version` bump
  (§3.3), decided when Phase 4B actually needs it, not guessed now.

### 3.10 Backward-compatibility policy

A RUBIC-family reader from a *newer* build encountering a file written
by an *older*, still-recognized `format_version` must interpret it
exactly per that version's own specification — never assume newer
fields are present, never require rewriting an old file just to read
it. This project's WAL already establishes this exact discipline (an
old-format segment remains readable by never being reinterpreted under
assumptions only a newer segment would satisfy); RUBIC inherits it, not
invents it separately.

### 3.11 Corruption handling principles

Identical, project-wide "torn write vs. corruption" classification to
the WAL's own (WAL Spec §6.3, carried into the LSM Engine Spec by §0.2):
only a file's own trailing, in-progress write can ever be legitimately
"torn" (expected, not alarming, truncate and proceed); anything else —
a corrupted checksum, an out-of-range length, an unrecognized structural
byte — is escalated as corruption, never silently discarded or
best-effort-repaired. A RUBIC-family reader never partially trusts a
corrupted block's contents; the already-specified RUBIC SSTable's own
answer to this (§2 above, "an SSTable is either fully valid or it does
not exist as far as the engine is concerned," backed by the atomic
tmp-file-then-rename construction discipline) is the concrete
instantiation of this shared principle for that format.

### 3.12 Reserved fields

Any RUBIC-family fixed-layout structure (a footer, a block header)
that includes explicitly reserved/padding bytes must document them as
reserved and require them to be all-zero on write; a reader
encountering non-zero reserved bytes in a recognized `format_version`
should treat this as corruption (per §3.11), not silently ignore it —
matching the WAL segment header's own existing `flags` field handling
(WAL hardening pass: "rejects... non-zero reserved `flags`").
**UNDEFINED — RESERVED FOR SSTABLE DESIGN**: the already-specified
RUBIC SSTable footer (§2 above) has no explicit reserved-byte padding
today (`8+4+8+8+8+8+8+8+8+4 = 72`, fully accounted for); whether a
future version adds any is not decided here.

### 3.13 Future extension mechanism

**UNDEFINED — RESERVED FOR SSTABLE DESIGN.** No RUBIC-family format
in this project currently has an established mechanism for forward-
compatible optional extension (e.g., a trailing variable-length
"extension block" a future version could append without breaking older
readers, versus the flat `format_version`-bump-for-any-change model
§3.3/§3.9 currently describe). This is deliberately left undecided
rather than guessed — Phase 4B's own RUBIC SSTable implementation work
is the appropriate place to decide it, once a real motivating use case
(e.g., per-block compression, which operating brief §37 explicitly
defers) exists to design against, per this project's own "calibrate
from measurement/real need, not speculation" principle.

## 4. Relationship to existing specifications

This document sits **above** `RubixDB-WAL-Specification-v1.0.md` and
`RubixDB-LSM-Engine-Specification-v1.0.md` as a governance/conventions
layer — it does not supersede, amend, or duplicate either. Where this
document and either of those specs could be read as disagreeing, the
component-level spec governs the byte-level detail and this document's
job is only to name and generalize the *pattern* those specs already
established (consistent endianness, consistent checksum algorithm,
consistent torn-vs-corrupt classification, etc.), not to introduce a
new one.
