# RubiXDB Phase 4B — Architecture Decision Records

## ADR-P4B-0: Proceed on the certification gate as explicitly provisional, mirroring Phase 4A's own precedent

**Status**: Accepted, documented, not hidden.

**Context**: Operating brief §2 requires checking whether Phase 3C and
Phase 4A are actually certified before implementing SSTable, and to
stop and ask only if an unresolved blocker *materially prevents safe
SSTable implementation*. Verified directly from source (not assumed):

- `PHASE3C_TEST_RESULTS.md` §12: **"Deferred"** — the long-duration
  soak, final performance re-verification, and two regression commands
  (`group_commit`, `crash_consistency` under `--release`) never
  completed; no certification decision was ever recorded.
- `PHASE4A_TEST_RESULTS.md` §12: **"MEMTABLE NOT YET READY FOR RUBIC
  SSTABLE IMPLEMENTATION — BLOCKERS REMAIN"**, explicitly, with two
  named blockers: (1) the full multi-threaded WAL-vs-WAL+MemTable
  performance comparison was never run (contaminated-machine
  precaution, `PHASE4A_ADR.md` ADR-P4A-6), and (2) Phase 3C's own
  certification (the item directly above) had not landed.

**Decision**: Proceed with Phase 4B on this explicitly-provisional
foundation, exactly as Phase 4A itself proceeded on top of a then-
incomplete Phase 3C (`PHASE4A_ARCHITECTURE.md` §0). Justification,
applied identically:

1. Phase 4B, like Phase 4A, does not modify any WAL/`GroupCommitter`/
   `BatchCoordinatorPool` internal (operating brief §3) — the soak
   continues to accumulate its own evidence independently of this
   phase's work, and nothing in this phase's own test results can be
   contaminated by, or contaminate, that soak.
2. Blocker (1) above is not merely carried forward unaddressed — it is
   **subsumed** by this phase's own mandatory requirement (operating
   brief §40-41) to measure WAL-only vs. WAL+MemTable vs.
   WAL+MemTable+SSTable-flush under the same clean-machine conditions.
   `PHASE4B_PERFORMANCE.md` records that comparison directly, closing
   this specific gap as part of this phase's own required work rather
   than leaving it open indefinitely.
3. Blocker (2) remains open and is carried forward honestly in
   `PHASE4B_TEST_RESULTS.md` — this document does not, and cannot,
   convert Phase 3C's own "Deferred" into a certification it never
   reached. Phase 4B's own final certification statement (operating
   brief §55) is explicitly conditioned on Phase 3C's eventually
   landing clean, exactly as `PHASE4A_TEST_RESULTS.md` already
   conditions itself on the same thing.

**Consequences**: No fabricated certification anywhere in this
project's history. A reader of `PHASE4B_TEST_RESULTS.md` sees the
exact same honest, explicit dependency chain a reader of
`PHASE4A_TEST_RESULTS.md` already sees.

---

## ADR-P4B-1: No Manifest, no WAL purge this phase — SSTable is a pure read-path addition

**Status**: Accepted (user decision, operating brief §31/§52's own
explicit "stop and ask" instruction was followed).

**Context**: `RubixDB-LSM-Engine-Specification-v1.0.md` §3.2 and §7
make the Manifest's `SET_CHECKPOINT` edit load-bearing for two things:
(a) safely bounding WAL retention (`wal.purge_before(flushed_through_
seq)`, an existing primitive verified present and tested since Phase
3C but with zero callers today) and (b) determining, on restart, which
WAL records are already durably represented by a published SSTable and
therefore need not be replayed. The operating brief explicitly
forbids implementing a Manifest this phase (§52) and explicitly
requires stopping to ask rather than guessing at a durability-
correctness boundary this significant (§31). Two options were
presented:

- **(A)** SSTable as a pure additional read-source; WAL untouched;
  recovery keeps doing full `wal::replay_streaming`, unchanged from
  Phase 4A.
- **(B)** A footer-derived implicit checkpoint (`max(max_seq)` across
  valid on-disk SSTables, no separate Manifest file) used to bound WAL
  replay and to call the existing `purge_before` after a flush
  publishes.

**Decision**: **(A)**, selected by the user explicitly. Option (B),
while technically avoiding a literal "Manifest" file, reimplements a
piece of the Manifest's own checkpoint-derivation role under a
different name and would require its own dedicated crash-matrix
(multi-SSTable ordering, crash after publish but before purge, purge
failure mid-flush) before it could be trusted as a durability
mechanism — exactly the kind of scope the operating brief's §52
("do not create a fake Manifest merely to make the architecture appear
complete") warns against, even without literally naming a file
"MANIFEST."

**Consequences** (see `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3 for
the full, itemized derivation):
- SSTable IDs are directory-scan-based, not Manifest-based (§3.1).
- Publication/discovery is "exists under `.sst` name and validates,"
  with no liveness-removal concept this phase (§3.3).
- A flush never calls `wal::purge_before` (§3.6) — WAL growth and
  full-replay cost are explicitly unbounded across restarts within
  this phase's scope, named as a limitation, not hidden.
- A corrupt or missing SSTable can never cause data loss this phase
  (§3.5) — the WAL, being untouched, remains the complete source of
  truth regardless of any SSTable's state. This is the direct, and
  materially valuable, safety corollary of choosing (A): it makes this
  phase's entire SSTable read path strictly additive to correctness,
  never a new way to lose data, only a new way to serve reads faster
  when available and to (once wired in Phase 4B's flush path) bound
  *in-memory* (not on-disk) growth by letting flushed immutable
  memtables be dropped from RAM.
- Flush's only durability effect is "the data is now also durably
  present in a second place" — it changes nothing about what recovery
  does or how much the WAL retains.

---

## ADR-P4B-2: SSTable liveness on open fails closed, not silently excludes

**Status**: Accepted.

**Context**: With no Manifest, "which `.sst` files are live" reduces to
"which `.sst` files exist and validate" (ADR-P4B-1). A corrupt `.sst`
file discovered at `LsmEngine::open` could either (a) be silently
excluded from the live set and startup continues, or (b) cause
`open()` to return `Err`, refusing to start.

**Decision**: **(b)** — fail closed. Operating brief §19 and §47 both
explicitly forbid silently skipping corruption ("do not silently skip
corruption," "no corrupted table treated as valid"). Because ADR-P4B-1
guarantees a corrupt SSTable never risks *data* loss (the WAL still
has everything), refusing to start costs only *availability*, and an
operator has a clear, documented remedy: move the offending `.sst`
file aside and reopen (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.3).
Silently excluding it instead would mean an operator could run
indefinitely with a database quietly missing read coverage for a
subset of historical data (still fully present in the WAL, but no
longer being served by the SSTable read path, and with no signal that
anything is wrong) — a strictly worse outcome for a bug that is, by
construction, always fully recoverable.

**Consequences**: `LsmEngine::open` propagates `sstable::open`'s
`Err(EngineError::Corruption { detail })` unchanged, with `detail`
naming the exact `.sst` path and which check failed (e.g. `"sstable
sstables/00000000000000000003.sst: footer checksum mismatch"`) — no
new `EngineError` variant is introduced (`src/error.rs`'s own doc
comment: one shared enum across every Phase 0 component), only a
descriptive `detail` string, so an operator does not have to guess
which file to move aside.

---

## ADR-P4B-3: `xxhash-rust` for the spec-mandated XXH64 bloom-filter hash

**Status**: Accepted (dependency policy §46 — full write-up).

- **Name**: `xxhash-rust`
- **Version**: pinned exact (matching this project's existing `=` pin
  style for `crc32c`/`proptest`/`criterion`/`static_assertions`)
- **Purpose**: implements the two independent 64-bit XXH64 hashes
  (seeds `0` and `1`) `RubixDB-LSM-Engine-Specification-v1.0.md` §2.4
  mandates by name for the bloom filter's double-hashing scheme.
- **Why the standard library is insufficient**: `std` has no hashing
  algorithm implementation beyond `SipHash` (via `DefaultHasher`, not
  exposed as a stable, seeded, spec-matching 64-bit primitive) — XXH64
  is an external, named algorithm the already-final spec requires
  bit-for-bit, not a hash function this project could substitute its
  own choice for without violating that spec.
- **Security implications**: XXH64 is a non-cryptographic hash, used
  here purely for bloom-filter bit placement (never for integrity —
  CRC32C, already a dependency, covers integrity per Section 2.5-2.7)
  and never on attacker-controlled data in a security-sensitive
  context beyond what any bloom filter already tolerates (a false
  positive is a wasted read, never a correctness or confidentiality
  issue, per the spec's own §2.4 note).
- **License**: dual MIT/BSL-1.0 (`xxhash-rust`'s published license) —
  permissive, no copyleft, compatible with this project's existing
  dependency surface (`crc32c` is Apache-2.0/MIT).
- **Performance reason**: XXH64 is one of the fastest non-cryptographic
  64-bit hashes available, relevant here because it runs once per key
  per bloom-filter insertion during every SSTable build (operating
  brief §39's write-throughput benchmark measures this cost directly).
- **Maintenance cost**: pure-Rust, zero transitive dependencies, no
  build script, no `unsafe` in the `xxh64` feature path used here —
  the lowest-maintenance-cost option that still satisfies the spec's
  exact algorithm requirement (evaluated against the spec-named
  alternative crate, `twox-hash`, which is equally valid but pulls in
  a broader multi-algorithm surface this project does not need; either
  would have been acceptable, `xxhash-rust`'s narrower, feature-gated
  API was preferred for exactly this reason).

**Consequences**: `Cargo.toml` gains one new `[dependencies]` entry,
`xxhash-rust = { version = "=0.8.18", features = ["xxh64"] }` (latest
published patch at implementation time, pinned exact per this
project's own convention) — the only new production dependency this
phase introduces.

---

## ADR-P4B-4: Reuse `wal::file_io::fsync_dir` via a one-line visibility export, not a second implementation

**Status**: Accepted.

**Context**: The atomic-construction discipline's directory-fsync step
needs the exact same Unix-real/Windows-no-op platform primitive the
WAL already implements and has already tested (`src/wal/file_io.rs`'s
`fsync_dir`, `pub(crate)` but unreachable outside `wal` because `mod
file_io;` itself is private). Writing a second, textually-identical
copy under `src/sstable/` would duplicate platform-specific logic this
project has already gotten right once (and already has a
`#[cfg(test)]` interception hook for), inviting drift between the two
copies over time.

**Decision**: Add `pub(crate) use file_io::fsync_dir;` to
`src/wal/mod.rs`. This is a visibility-only change — `fsync_dir`'s
signature, behavior, and every existing call site are unchanged; it
does not touch the WAL binary format, `GroupCommitter`,
`BatchCoordinatorPool`, `durable_through`, sequence assignment, or
MemTable semantics (operating brief §3's preservation list). `src/
sstable/mod.rs` calls `crate::wal::fsync_dir(...)` directly.

**Consequences**: Windows/Linux behavior and its documented gap
(`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §4) are identical to the
WAL's own, by construction, not by parallel re-implementation.

---

## ADR-P4B-5: Background flush thread, bounded by the existing `max_immutable_memtables` backpressure

**Status**: Accepted — this is not a new decision, it is the already-
specified design (`RubixDB-LSM-Engine-Specification-v1.0.md` §4.4
marks the SSTable-write step "(background)" explicitly) finally
implemented; `PHASE4A_ADR.md` ADR-P4A-5 already named this exact gap
("Phase 4A has no flush-to-SSTable path to actually drain the
immutable list ... becomes materially less severe once Phase 4B adds
one") as the reason Phase 4A's own backpressure trade-off was
acceptable only temporarily.

**Decision**: A single dedicated background flush thread (mirroring
the existing single-dedicated-thread pattern the Batch Coordinator
already established for the write path — no new concurrency-control
novelty introduced) drains `immutables` oldest-first: for each frozen
`Arc<MemTable>`, build its SSTable (temp file -> fsync -> rename ->
directory fsync, `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.4), then
publish it into `LsmEngine`'s `sstables` list and drop that entry from
`immutables`. `max_immutable_memtables` (already a configured, tested
bound since Phase 4A) is the flush-backpressure bound operating brief
§42-43 asks for: a write that would need to freeze beyond that bound
still returns `EngineError::CapacityExceeded` (ADR-P4A-5's existing,
documented trade-off, now transient rather than permanent — it clears
as soon as the flush thread catches up), never blocks indefinitely,
never silently drops an immutable memtable, and never falsely
acknowledges a write (the write was already durable via the WAL before
this bound is ever checked, unchanged from Phase 4A).

**Consequences**: One new background thread, joined cleanly on
`LsmEngine` shutdown (extends the existing shutdown delegation
`PHASE4A_FAILURE_MODEL.md` §1 already documents, rather than
introducing a second, divergent shutdown path).
