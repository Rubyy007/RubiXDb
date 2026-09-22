# PHASE_RELATIONAL_TRANSACTION_STORAGE_RESULTS

**Scope**: `LsmEngine::write_batch` — the atomic multi-key storage
primitive specified by `PHASE_RELATIONAL_DATABASE_ADR.md`'s D9 and fully
resolved by `RELATIONAL ADR AMENDMENT 001` (AA.1–AA.9), implemented per
`PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md`. This document records
what was actually built, measured, and verified — nothing beyond it.

**RELATIONAL DATABASE PRODUCTION READY = NO.** This increment adds one
storage primitive. No catalog, schema, table, index, SQL, transaction
executor, or CLI code exists.

---

## 1. Final `write_batch` semantics

```rust
pub enum WriteOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

impl LsmEngine {
    pub fn write_batch(&self, ops: &[WriteOp]) -> Result<u64>;
}
```

- One shared, durable sequence per batch (not a contiguous range) —
  assigned at the WAL frame level exactly as `put`/`delete` already
  assign theirs, `self.next_seq += 1` once per `write_batch` call
  regardless of `ops.len()`.
- Empty batch rejected before any encoding: `Err(EngineError::
  InvalidArgument { .. })`.
- Batch exceeding `LsmConfig::max_batch_ops` (default 10,000, matching
  D27's relational-layer default) rejected before any encoding:
  `Err(EngineError::CapacityExceeded { .. })`.
- Same-key operations resolve deterministically: last operation in the
  caller-supplied slice wins, via ordinary `MemTable`/`BTreeMap`
  overwrite at the shared `(key, seq)` tuple — no dedup pass, no special
  casing.
- Either every operation becomes durable and visible, or none do — no
  partial-batch state is ever observable by any reader (§4 below).

## 2. WAL format

One new op tag, `OP_GROUP = 5` (`src/wal/format.rs`), additive to the
existing frame envelope (`length || crc32c || seq || op_tag || op_body`
— unchanged). `op_body` for a `Group`:

```
member_count:u32 LE || member*
member := member_tag:u8 (OP_PUT | OP_DELETE) || member_body (identical
          layout to today's standalone PUT/DELETE op_body)
```

`PUT`(1)/`DELETE`(2)/`CHECKPOINT_MARKER`(3) are byte-for-byte unchanged.
`wal::recovery::walk_segment`/`classify_failure` required **zero
changes** — they classify frames purely by `length`/`crc32c`, never
`op_tag`, so a `Group` frame's torn-tail/corruption handling is the
existing, unmodified rule, verified by re-running the full existing
`wal_tests`/`pathological_recovery_matrix` suites unmodified (§6).

New Rust types (`src/wal/ops.rs`): `GroupMember<'a>`/`GroupMemberOwned`
— exactly two variants each (`Put`/`Delete`), no `Group` variant of
their own and no `CheckpointMarker` variant, so nesting is a **compile
error** on the encode side. `WalOp<'a>` gained a `Group { members:
Vec<GroupMember<'a>> }` variant (no longer `Copy`, still `Clone` — every
existing call site constructs and consumes a `WalOp` once, inline; no
call site relied on implicit copying, confirmed by the compiler across
the whole workspace).

**Decode-side anti-DoS discipline** (the one genuinely new security-
relevant mechanism): `decode_wal_body`'s `Group` arm grows its output
`Vec<GroupMemberOwned>` via `.push()` in a loop, **never**
`Vec::with_capacity(member_count as usize)` — `member_count` is an
unvalidated `u32` read directly from on-disk bytes, and pre-allocating
by it would be exactly the attacker-controlled unbounded-allocation
path this phase's security bar forbids. A bogus huge `member_count`
simply exhausts the already-`max_record_len`-bounded frame body and
returns `Corruption` after at most a few iterations. Verified by a
dedicated test (`group_decode_rejects_oversized_member_count_
gracefully`) crafting `member_count = u32::MAX` with genuinely few
trailing bytes.

## 3. Sequence semantics

Verified directly against source (`src/wal/mod.rs:806-828`,
`src/memtable/mod.rs:117-121`): `FileWal::append` assigns exactly one
`seq` per call and increments its counter by exactly one, regardless of
op kind. `MemTable`'s map key is `(user_key, seq)`. A shared seq across
all N members is therefore the direct, forced consequence of the
existing on-disk record layout and the existing versioned-map model —
not a simplification chosen for convenience.

## 4. Atomic visibility — proof, and how it was tested

`apply_batch_after_durable` (`src/lsm/mod.rs`) takes the *same*
`RwLock<MemTable>` write guard `apply_after_durable` already takes for
one insert, held for all N inserts in one critical section. `RwLock`'s
mutual-exclusion guarantee means any reader's read-guard acquisition is
strictly before or strictly after the writer's, never during — so a
reader observes zero or all of a batch's effects, never a partial
subset. No new lock was introduced (confirmed: `apply_batch_after_
durable` acquires exactly one lock, the pre-existing `active` field's).

**Tested directly**, not merely reasoned about: `lsm::tests::
write_batch_tests::concurrent_reader_never_observes_a_partial_batch`
races a reader's single `range_scan` (one lock acquisition covering all
four of a batch's keys, via `capture_read_view`) against a `write_batch`
call deliberately held mid-critical-section (`set_batch_apply_delay_
for_test`, mirroring the codebase's own established `set_flush_delay_
for_test` pattern), synchronized via `std::sync::Barrier`, across 30
repeated interleavings. Zero partial observations. (An earlier draft of
this test used four *independent* `get_as_of` calls instead of one
`range_scan`; that version could observe legitimate cross-call
straddling — a real property of issuing four separate, independently-
lock-acquiring reads, not a partial-batch-visibility bug — and was
replaced once the distinction was understood, rather than weakened to
pass. See the test's own doc comment for the full account.)

**Flagged, not silently resolved** (per `RELATIONAL ADR AMENDMENT 001`
AA.3): `durable_through` can advance before a specific in-flight
caller's own apply step runs — a pre-existing, unchanged property of
the certified write pipeline, inherited unmodified by `write_batch`. It
affects *when* an unrelated concurrent snapshot may observe a
still-in-flight write, never whether a reader can see part of one.

## 5. Crash recovery

`apply_wal_op`'s new `Group` arm and `apply_batch_after_durable` both
call the *same* new helper, `apply_kv_to_memtable`, in the same caller-
supplied order — recovery-replay and live-apply are provably identical,
not independently-maintained-but-hopefully-equivalent. Verified by:
`write_batch_survives_restart_all_members_present` (all N members
present after a real engine close/reopen) and `write_batch_recovery_
preserves_same_key_last_wins_resolution` (a same-key Put/Put batch
resolves identically before and after restart). The full existing
`pathological_recovery_matrix` (9 fixtures) and `crash_consistency`
(cross-process abort-point) suites were re-run unmodified and pass —
neither needed a `Group`-specific fixture added to prove the frame-
level torn/corrupt handling, since that handling is unmodified, generic
code (§2).

## 6. Concurrency integration

`write_batch` submits exactly one `WalOpOwned::Group(members)` through
the existing, unmodified `BatchCoordinatorPool::submit`/`GroupCommitter`
— no second batching mechanism. `estimate_frame_len` (queue-byte-budget
accounting) was extended with a `Group` arm, verified against the real
encoder byte-for-byte (`estimate_frame_len_matches_the_real_encoder_
for_group`), so large batches correctly consume more of the existing
`max_queued_bytes` backpressure bound.

## 7. Resource limits

| Limit | Value | Enforced |
|---|---|---|
| Max operations per batch | `LsmConfig::max_batch_ops`, default 10,000 | `write_batch`, before any encoding |
| Max encoded frame bytes | `WalConfig::max_record_len` (reused, unchanged, default 64 MiB) | `format::encode_frame`'s existing check |
| Max key/value bytes | Same bound, per field | `write_len_prefixed`'s existing per-field check, reused for every `Group` member |

Verified: `write_batch_at_exactly_max_batch_ops_succeeds`,
`write_batch_over_max_batch_ops_fails_with_capacity_exceeded_and_
writes_nothing` (confirms **zero** partial writes on rejection — every
key checked absent afterward), `group_encode_rejects_oversized_member_
key_with_capacity_exceeded`.

## 8. Security audit

- **No new `unsafe`** anywhere in the diff (`git diff -- src/ api/` —
  grepped, zero matches).
- **No new logging of keys/values/credentials** — zero new
  `println!`/`eprintln!`/`log::`/`tracing::` calls in any changed
  source file (grepped the diff directly).
- **No new external dependency** — `Cargo.toml`'s only change is a
  `[[bench]]` registration.
- **No unchecked allocation from untrusted input** — §2's decode-side
  discipline is the concrete mechanism; verified by
  `group_decode_rejects_oversized_member_count_gracefully` and
  `group_decode_rejects_oversized_member_count_with_leftover_garbage`
  (both complete in milliseconds, not a hang or an allocation spike).
- **No new panic paths** — `write_batch`'s and `decode_wal_body`'s
  `Group` arm bodies contain zero `.unwrap()`/`.expect()`/`panic!`
  (grepped directly); every fallible step propagates via `?`.
- **`EngineError::InvalidArgument`**, the one new error variant, follows
  the same `Display`/detail-only discipline every existing variant
  already follows (no path, no raw OS error text) — and its one
  cross-crate consequence (`api/src/error.rs`'s exhaustive match needed
  one new arm to keep the workspace compiling) is mapped to `400
  VALIDATION_ERROR`, consistent with the existing API error-code
  convention, with a dedicated test.
- **Trust boundary unchanged**: `write_batch` is an internal
  `LsmEngine` method, not exposed over any API surface in this
  increment — no new authentication/authorization surface exists yet
  to audit.

## 9. Benchmarks (measured, `cargo bench --bench write_batch_bench`, this
machine, release profile — raw Criterion output, not summarized away)

### N=1 parity

| Operation | Mean time |
|---|---|
| `put` | 6.24 ms [6.15, 6.34] |
| `write_batch([Put])` | 6.39 ms [6.08, 6.74] |
| `delete` | 12.92 ms [12.75, 13.13] |
| `write_batch([Delete])` | 12.99 ms [12.81, 13.23] |

Both pairs overlap within their own confidence intervals — **no
material regression**, confirming AA.5's required N=1 parity property
by measurement, not assumption.

### N>1 throughput vs. an equivalent serialized baseline

| N | serialized (N sequential `put`) | `write_batch` | speedup |
|---|---|---|---|
| 2 | 12.42 ms | 5.49 ms | 2.3x |
| 4 | 26.70 ms | 4.57 ms | 5.8x |
| 8 | 52.15 ms | 4.29 ms | 12.2x |
| 16 | 104.23 ms | 4.26 ms | 24.5x |
| 32 | 159.39 ms | 4.41 ms | 36.2x |
| 64 | 271.62 ms | 4.74 ms | 57.3x |

`write_batch`'s time stays roughly constant (~4.2–5.5 ms, dominated by
one `fsync` — consistent with this machine's own documented ~3–10 ms
`Immediate`-mode `fsync` latency, `PROGRESS.md`) regardless of N, while
the serialized baseline grows linearly (each sequential `put` from one
caller pays its own full durability wait). This directly confirms AA.5's
second required property: "atomicity did not destroy throughput;
batching actually reduces durability overhead" — measured, not claimed.

WAL-bytes-per-operation was **not** separately measured; it follows
analytically from the format in §2 (one frame header + one `seq` instead
of N) and was not treated as needing an independent empirical check.

## 10. Full regression gate

| Suite | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo check --workspace --all-targets --all-features` | clean |
| `cargo test --lib` | 373 passed, 0 failed |
| `cargo test --release --lib` | 373 passed, 0 failed |
| `cargo test --test wal_tests` | 12 passed, 0 failed |
| `cargo test --test pathological_recovery_matrix` | 9 passed, 0 failed |
| `cargo test --test crash_consistency --features test-util` | 2 passed, 0 failed |
| `cargo test --test group_commit` | 3 passed, 2 failed (pre-existing) |
| New `write_batch`-specific tests (`lsm::tests::write_batch_tests::*`) | 14 passed, 0 failed |
| New WAL `Group`-frame tests (`wal::ops::tests::group_*`, `wal::group_commit::tests::estimate_frame_len_matches_the_real_encoder_for_group`) | 13 passed, 0 failed |

**Flagged, investigated, confirmed pre-existing** (not a regression):
`group_commit`'s `m1_2_hundred_writers_throughput` and `m1_3_thousand_
writers_throughput` fail on this machine in both debug and `--release`
mode (7,330 ops/sec vs. a 15,000 target; 45,395–52,460 ops/sec vs. an
80,000 target). Verified by `git stash`-ing every change in this
increment and re-running the identical suite against the unmodified
baseline: **the same two tests fail identically**, with near-identical
numbers, on the clean checkout. This is a pre-existing characteristic
of this development machine relative to whatever hardware these
thresholds were calibrated on (the tests' own failure messages say
"reproduce with: cargo test --release"), not something this increment
introduced or should paper over. Not touched, not weakened, per the
review directive's own explicit instruction.

## 11. Protected-path audit

```
git diff --stat -- src/manifest/   → (empty)
git diff --stat -- src/compaction/ → (empty)
git diff --stat -- src/sstable/    → (empty)
```

Changed: `src/error.rs` (+1 variant), `src/wal/{format,ops,group_
commit,mod,recovery}.rs` (Group frame support + two pre-existing tests'
`.clone()` fixes forced by `WalOp` losing `Copy`), `src/lsm/{mod,tests}.
rs` (`write_batch`/`WriteOp`/`LsmConfig::max_batch_ops` + new tests),
`api/src/error.rs` (one mechanical error-mapping arm, required for the
workspace to compile — not a feature addition), `Cargo.toml` (one new
`[[bench]]` entry), `benches/write_batch_bench.rs` (new).

No catalog, schema, table, index, SQL, transaction-executor, or CLI code
exists anywhere in this diff.

---

## Certification

**WRITE BATCH = PASS.**

**RELATIONAL IMPLEMENTATION = NOT STARTED beyond this required storage
extension.**

**RELATIONAL DATABASE PRODUCTION READY = NO.**

**WRITE ENGINE = PRODUCTION READY**
**READ ENGINE = PRODUCTION READY**
**COMPACTION = PRODUCTION READY**

Increment 3 (persistent relational catalog + database/schema/table
model) may now begin, per `PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md`
§4/§5.
