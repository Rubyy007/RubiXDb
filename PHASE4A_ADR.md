# RubiXDB Phase 4A — Architecture Decision Records

## ADR-P4A-1: Follow the existing, final LSM Engine Spec exactly for the MemTable's own data structure and layout

**Status**: Accepted, implemented.

**Context**: Operating brief §10 asks to evaluate `BTreeMap` against
`SkipList` "if the specification permits both." `RubixDB-LSM-Engine-
Specification-v1.0.md` §1.1 is "Status: Final — ready for
implementation" and already prescribes `BTreeMap<(Vec<u8>, u64),
MemtableValue>` by exact type, with `get_as_of` built directly on its
`range(...).next_back()` API.

**Decision**: Implement exactly as specified. No `SkipList` benchmark
was run.

**Why not run the comparison anyway, "just to be thorough"?** Per this
project's own established practice across every prior WAL/Group-Commit
phase — a settled Tier-1 specification is implemented as written, not
re-litigated absent a discovered defect (`ARCHITECTURE.md`'s own
"Decide-vs-Ask Policy" framing; `PHASE4A_MEMTABLE_ARCHITECTURE.md` §2
applies the identical reasoning to this exact decision) — re-opening an
already-decided, "Final" specification
without a concrete reason is itself a process risk: it invites drift
between what's specified and what's built, and spends effort on a
question the project has already answered. A `SkipList` would also
require either `unsafe` code (a hand-rolled lock-free variant) or a new
dependency (e.g. `crossbeam-skiplist`) — both gated behind this
project's own Tier 3 stop-and-ask policy, and neither justified without
a measured problem `BTreeMap` demonstrably has. `examples/memtable_
bench.rs`'s own real numbers (~1.45M puts/sec single-threaded, p50 get
latency 400ns) show no such problem exists at the scale this phase
operates at.

## ADR-P4A-2: MemTable applied on the caller's own thread, not the coordinator thread — corrected mid-design

**Status**: Accepted, implemented (see `PHASE4A_MEMTABLE_ARCHITECTURE.md`
§3 for the full account, including the specific inaccurate claim this
ADR corrects).

**Context**: An early draft of this phase's own architecture doc
assumed the Dedicated Batch Coordinator's single thread would be the
only caller of `MemTable::insert`, mirroring how it is the sole
`fsync`-calling thread for the WAL. On closer inspection, `Completion::
wait()` returns control to *the original submitting thread*, not the
coordinator thread — so applying the MemTable insert immediately after
that call, on that same thread, means multiple concurrent callers can
indeed call `insert` around the same time.

**Decision**: Accept this design (apply on the calling thread,
synchronized by `RwLock<MemTable>`) rather than either (a) forcing the
insert to happen on the coordinator thread instead (would require
threading MemTable-specific logic into `execution::batch_coordinator`'s
own `process_batch`, touching certified, soak-validated code — against
operating brief §2's preservation instruction, for no correctness
benefit) or (b) adding a dedicated background "applier" thread/channel
(unjustified new complexity for a problem that does not exist — see
below).

**Why this is still correct**: `MemTable`'s map is keyed by the full
`(user_key, seq)` pair, and every `seq` is globally unique (WAL-
assigned, operating brief §8). Two different entries' `insert` calls
can never target the same map key, so the relative order in which
concurrent callers acquire the write lock and insert **does not affect
the resulting map's logical content** — `BTreeMap` insertion of
distinct keys commutes. The single-writer principle the `MemTable` type
itself relies on (spec §1.5: no *internal* synchronization) is about
never mutating one instance from two threads *without* external
synchronization — `RwLock` supplies exactly that, regardless of which
thread calls in.

**Verified, not just argued**: `src/lsm/tests.rs`'s concurrency tests
(1/10/100/1,000 logical writers, operating brief §31) confirm zero lost
records, zero duplicate/incorrect state, across real concurrent
completion/insert interleaving.

## ADR-P4A-3: Bounded-memory recovery via `wal::replay_streaming` — a new, additive WAL function, not a modified one

**Status**: Accepted, implemented (`PHASE4A_MEMTABLE_ARCHITECTURE.md`
§10 has the full design).

**Context**: `PHASE3C_ADR.md` ADR-P3C-1 already analyzed three
directions (streaming iterator, callback replay, bounded replay
batches) for fixing the WAL recovery API's own unbounded-memory
materialization, without implementing any of them, per that ADR's own
explicit deferral. Operating brief §24 requires starting from that
analysis rather than inventing a fourth mechanism, and requires the
result to be "a separately documented WAL increment," never a silent
change to `open_for_recovery`.

**Decision**: Implement the callback-replay direction — `wal::
replay_streaming(dir, config, on_record)` — as new code alongside the
unmodified `open_for_recovery`/`inspect`/`WalReplayResult`/
`walk_segment`/`scan_directory`. Reuses the exact same per-segment
scanning primitives those functions already use; the only difference is
whether a segment's own `Vec<(u64, WalOpOwned)>` is accumulated into one
combined directory-wide `Vec` (the existing functions' behavior,
unchanged) or streamed through a callback and dropped per-segment (the
new function's behavior).

**A real bug found and fixed while implementing this**: the first
version called `canonicalize_existing_dir` (never creates a directory,
matching `inspect`'s own read-only contract) unconditionally — which
fails on a brand-new `LsmEngine`'s not-yet-created WAL directory.
Calling `FileWal::open_for_recovery` first to guarantee the directory
exists was not a viable fix either: its exclusive lock would block
`replay_streaming`'s own shared-lock attempt, **even from the same
process** (`ARCHITECTURE.md`'s "Cross-process file locking" section
documents this exact same-process-blocks-itself property, verified
empirically when that locking mechanism was first built). Fixed by
treating a not-yet-existing directory as "nothing to replay" — the same
outcome `scan_directory`'s own `ids.is_empty()` branch already produces
for an *existing* empty directory, extended to cover "doesn't exist
yet" too. A dedicated regression test
(`replay_streaming_on_a_never_created_directory_is_empty_not_an_error`)
locks this in.

**Consequences**: `LsmEngine::open` calls `replay_streaming` *before*
`FileWal::open_for_recovery`, by necessity (the lock-ordering
constraint above) — documented explicitly in `src/lsm/mod.rs`'s own
`open` method, not left as an unexplained ordering choice.

## ADR-P4A-4: `DEFAULT_MAX_SIZE_BYTES` follows the spec's actual value (4 MiB), not the operating brief's "~64 MiB" description

**Status**: Accepted, flagged not silently resolved.

**Context**: Operating brief §18 states "the prior design used
approximately 64 MiB, if that remains the current specification."
`RubixDB-LSM-Engine-Specification-v1.0.md` §4.1's own `LsmConfig::
memtable_max_size_bytes` default is actually documented as **4 MiB**.
There is no "prior design" for the operating brief to be describing —
this is the very first MemTable implementation in this project's
history; no earlier MemTable code or configuration ever existed to set
a 64 MiB precedent. The 64 MiB figure most plausibly conflates this
with `wal::format::DEFAULT_MAX_SEGMENT_SIZE` (which genuinely is 64
MiB) — a different component entirely.

**Decision**: Use the spec's own actual value, 4 MiB
(`memtable::DEFAULT_MAX_SIZE_BYTES`), per this project's own standing
rule that a settled, "Final" Tier-1 specification governs over an
operating brief's own recollection of it, and flag the discrepancy in
both the constant's own doc comment and here — never silently
substitute one number for the other without saying so.

**Consequences**: None to existing code (no prior MemTable existed to
have used a different default). `LsmConfig::memtable_max_size_bytes`
remains fully configurable (operating brief §18's own requirement) —
this decision only fixes the *default*, not a hard-coded, unchangeable
value.

## ADR-P4A-5: Immutable-memtable backpressure reports failure for the triggering write, not the freeze specifically

**Status**: Accepted, documented as a known trade-off, not treated as
an urgent defect.

See `PHASE4A_FAILURE_MODEL.md` §2 for the full account: when a write
makes the active MemTable full and freezing it would exceed `max_
immutable_memtables`, `LsmEngine::put`/`delete` returns `Err` even
though the underlying write is already durable and already applied.
Accepted this phase because Phase 4A has no flush-to-SSTable path to
actually drain the immutable list — this specific interaction becomes
materially less severe once Phase 4B adds one (backpressure becomes
transient rather than a hard wall). Revisiting the exact error contract
at that point, with real flush-latency data to calibrate against, is
more appropriate than guessing at a better contract now.

## ADR-P4A-6: WAL-vs-WAL+MemTable performance comparison deferred — not fabricated under contamination

**Status**: Accepted (explicit, documented gap — see `PHASE4A_
PERFORMANCE.md`).

**Context**: The background Phase 3C long soak (`examples/long_soak_
test.rs`) was still running throughout this phase's implementation
work. A quick 20-writer `examples/lsm_load_test.rs` smoke run returned
an obviously contaminated 1,479 ops/sec (vs. an expected order of
magnitude higher) — the identical CPU-contention signature `PHASE3C_
TEST_PLAN.md` §1 rule 5 already diagnosed and added a rule against
trusting.

**Decision**: Do not run, and do not fabricate or extrapolate, the
full multi-threaded 100/1,000-writer WAL-only-vs-WAL+MemTable
comparison operating brief §32/§36 asks for while the machine is not
idle. `examples/memtable_bench.rs`'s single-threaded, CPU-bound numbers
(not meaningfully distorted by scheduling contention the way many-
thread benchmarks are) stand as legitimate evidence that MemTable's own
per-operation cost is negligible relative to WAL `fsync` latency (~400
ns vs. several milliseconds) — but the *combined*, real-world-shaped
comparison remains an open item, recorded honestly in `PHASE4A_TEST_
RESULTS.md` §9 and §11 rather than guessed at.
