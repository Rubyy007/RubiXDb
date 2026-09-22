# PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR

**Scope**: this document is narrowly scoped to exactly one thing:
`LsmEngine::write_batch`, the storage primitive D9 named and
`RELATIONAL ADR AMENDMENT 001` (`PHASE_RELATIONAL_DATABASE_ADR.md`,
AA.1–AA.9) fully specified. It does not re-derive any of that
specification — it cites it and records the implementation's concrete
file-by-file plan. No catalog, schema, table, index, SQL, transaction
executor, or CLI code is added in this increment. Only the storage
primitive.

**Authoritative specification**: `PHASE_RELATIONAL_DATABASE_ADR.md`,
`RELATIONAL ADR AMENDMENT 001`, sections AA.1 (sequence semantics),
AA.2 (WAL frame format), AA.3 (atomic-visibility proof), AA.4 (same-key
resolution), AA.5 (performance requirement), AA.6 (resource limits),
AA.7 (failure semantics), AA.8 (concurrency integration), AA.9
(security review). Every design choice below is a direct execution of
one of those sections — cited inline, not repeated.

---

## 1. Public API surface added

`src/lsm/mod.rs`:

```rust
/// One mutation inside an atomic `write_batch` call — AA.1/AA.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

impl LsmEngine {
    /// Atomically applies N operations under one shared, durable
    /// sequence — AA.1–AA.9 define the complete contract.
    pub fn write_batch(&self, ops: &[WriteOp]) -> Result<u64>;
}
```

`LsmConfig` gains one new field: `pub max_batch_ops: usize` (default
`10_000`, AA.1/AA.6).

`src/error.rs` gains one new `EngineError` variant:
`InvalidArgument { detail: String }` (AA.1's empty-batch rejection,
AA.9). Its cross-crate consequence — `api/src/error.rs`'s existing
exhaustive `EngineError` match must gain one new arm — is accounted for
in §6 below; this is the only change outside the engine crate this
increment makes, and it is mechanical (a compile-time requirement, not
a feature addition).

## 2. WAL layer changes (AA.2)

- `src/wal/format.rs`: `pub const OP_GROUP: u8 = 5;`
- `src/wal/ops.rs`:
  - `GroupMember<'a> { Put { key: &'a [u8], value: &'a [u8] }, Delete {
    key: &'a [u8] } }` (non-recursive by construction — AA.2).
  - `GroupMemberOwned { Put { key: Vec<u8>, value: Vec<u8> }, Delete {
    key: Vec<u8> } }`.
  - `WalOp<'a>::Group { members: &'a [GroupMember<'a>] }`,
    `WalOpOwned::Group(Vec<GroupMemberOwned>)`, with
    `WalOpOwned::as_wal_op` extended by one arm, mirroring the existing
    `Put`/`Delete`/`CheckpointMarker` arms exactly.
  - `encode_wal_frame`: new `Group` match arm — writes `member_count:u32
    LE` then each member's tag + body via the existing
    `write_len_prefixed`, unchanged.
  - `decode_wal_body`: new `OP_GROUP` match arm — reads `member_count`,
    decodes that many members via `.push()` into a `Vec::new()` (never
    `Vec::with_capacity(member_count)` — AA.2's load-bearing anti-DoS
    rule), rejects any member tag other than `OP_PUT`/`OP_DELETE` as
    `Corruption`.
- `src/wal/group_commit.rs`: `estimate_frame_len` gains a matching
  `Group` arm (AA.2), with a byte-for-byte regression test against
  `encode_wal_frame`'s real output, mirroring the existing
  `estimate_frame_len_matches_the_real_encoder_for_put` test.
- `src/wal/recovery.rs`: **no changes.** `walk_segment`/`classify_
  failure` operate purely on frame `length`/`crc32c`, never inspecting
  `op_tag` — a `Group` frame is torn/corrupt-classified by the existing,
  unmodified code (AA.2).

## 3. LSM engine layer changes (AA.1, AA.3, AA.7)

`src/lsm/mod.rs`:

- `write_batch`: validates `ops.is_empty()` →
  `InvalidArgument`; `ops.len() > self.config.max_batch_ops` →
  `CapacityExceeded`; then `self.reject_if_storage_full()?` (mirroring
  `put`/`delete`'s existing pre-check); then builds `Vec<GroupMember>`
  from `ops`, submits `WalOpOwned::Group(...)` via the existing
  `self.pool.submit(...)`, waits via the existing
  `completion.wait()?`, then calls the new `apply_batch_after_durable`.
- `apply_batch_after_durable(&self, ops: &[WriteOp], seq: u64) ->
  Result<()>`: takes `self.lock_active_write()` **once**, inserts all N
  entries under `seq` in caller-supplied order (AA.4's last-wins
  overwrite semantics fall out of this for free), then runs the
  existing `is_full()`/`freeze_locked` check exactly once, after all N
  inserts — mirroring `apply_after_durable` exactly, generalized from 1
  insert to N inside the same critical section (AA.3's proof).
- `apply_wal_op` (recovery replay, `src/lsm/mod.rs:3146`): new `WalOp::
  Group { members }` arm — applies every member under the frame's one
  decoded `seq`, via the *same* per-member application the live path
  uses (a shared private helper, not a duplicated match), so live-apply
  and recovery-replay are provably identical (AA.1).

## 4. Increments explicitly NOT included here

Per the review directive's §19 and this project's own controlled-
increment discipline: no catalog, no `system.*` tables, no schema/table/
column objects, no indexes, no SQL parser/binder/planner/executor, no
CLI, no transaction executor (D10's Snapshot-Isolation commit path is
architecture only until this primitive is certified). Increment 3
(persistent relational catalog) may not begin until this document's own
test/regression gate (§5) passes in full and
`PHASE_RELATIONAL_TRANSACTION_STORAGE_RESULTS.md` records that outcome.

## 5. Certification gate for this increment

`write_batch` is considered done only once **all** of the following
pass, each cited to its defining amendment section:

- Every AA.1–AA.9 "Testing requirements" list, in full (not a subset).
- N=1 performance parity and the N∈{2,4,8,16,32,64} throughput benefit,
  measured and recorded with raw numbers (AA.5) — not claimed without
  measurement.
- The full existing certified regression suite unmodified and still
  passing: `cargo fmt --check`, `cargo clippy --all-targets
  --all-features -- -D warnings`, `cargo test --lib`, `cargo test
  --release --lib`, `cargo check --all-targets --all-features`,
  `wal_tests`, `crash_consistency`, `pathological_recovery_matrix`,
  Read Engine tests, Compaction tests.
- Protected-path audit: `git diff --stat` and `git diff -- src/wal/
  src/manifest/ src/error.rs src/compaction/ src/sstable/ src/lsm/`
  show only the minimum `write_batch`-implementing changes listed in
  §1–§3 above — no unrelated engine behavior touched.
- Security audit (AA.9): no new `unsafe`, no unchecked allocation
  (specifically: no `Vec::with_capacity` sized by an untrusted decoded
  integer, per §2's `decode_wal_body` rule), no new external
  dependency, no key/value/credential logging, no filesystem-path or
  raw-I/O leakage in any new error path.

Results are recorded in `PHASE_RELATIONAL_TRANSACTION_STORAGE_RESULTS.md`,
which does **not** claim `RELATIONAL DATABASE PRODUCTION READY` — only
`WRITE BATCH = PASS/FAIL`, scoped exactly to this primitive.
