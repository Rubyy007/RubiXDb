//! LSM engine facade — write-path integration (`Logical Client ->
//! Dedicated Batch Coordinator -> Group Commit -> Durable WAL -> Mutable
//! MemTable -> Immutable MemTable -> RUBIC SSTable`), per
//! `PHASE4A_ARCHITECTURE.md` and `PHASE4B_ARCHITECTURE.md`. **Not** the
//! full `RubixDB-LSM-Engine-Specification-v1.0.md` §4 facade — no
//! `manifest` field, no compaction. `sstables`/`next_sstable_id` were
//! added in Phase 4B; `manifest` and compaction remain future phases
//! (`PHASE4B_ADR.md` ADR-P4B-1, operating brief §52-§53).
//!
//! # Durability
//!
//! This module introduces **no second durability system**, and Phase 4B
//! does not change that: `MemTable::insert` is still called only after
//! `Completion::wait()` confirms WAL durability
//! (`PHASE4A_ARCHITECTURE.md` §5), and the RUBIC SSTable flush path added
//! this phase is a purely derived, purely additive read-path source over
//! data the WAL already holds durably — it is never the sole durable
//! copy of anything, and it never truncates or purges the WAL
//! (`PHASE4B_ADR.md` ADR-P4B-1). `LsmEngine::open`'s recovery step
//! (`wal::replay_streaming`) reconstructs the MemTable from the WAL
//! exactly as Phase 4A already did, unchanged by anything in this file.
//!
//! # Concurrency
//!
//! See `PHASE4A_MEMTABLE_ARCHITECTURE.md` §3 for the full account of the
//! active `MemTable`'s `RwLock`-guarded, any-caller-thread apply pattern
//! (unchanged). `immutables` and `sstables` are each `Arc<RwLock<...>>`
//! so the background flush thread (`PHASE4B_ADR.md` ADR-P4B-5) can share
//! and mutate them without borrowing `LsmEngine` itself across a thread
//! boundary.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BinaryHeap, HashSet, VecDeque};
use std::io;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::compaction::CompactionStats;
use crate::error::{EngineError, Result};
use crate::execution::batch_coordinator::{BatchCoordinatorConfig, BatchCoordinatorPool};
use crate::manifest::{self, Manifest, ManifestEdit, ManifestState};
use crate::memtable::{MemTable, MemtableValue, DEFAULT_MAX_SIZE_BYTES};
use crate::sstable::{
    self, RecordValue, SsTable, SsTableRangeCursor, SsTableWriterConfig, SstableMeta,
};
use crate::wal::{self, FileWal, GroupCommitter, Wal, WalConfig, WalOp, WalOpOwned};

/// `RubixDB-LSM-Engine-Specification-v1.0.md` §4.1's `LsmConfig`, plus
/// the Phase-4B fields it also defines (`sstable_target_block_size`,
/// `bloom_bits_per_key`) and, as of `ADR-COMPACTION-001` §25,
/// `compaction_trigger_count`.
#[derive(Debug, Clone)]
pub struct LsmConfig {
    /// LSM Engine Spec §4.1's own `memtable_max_size_bytes` default (4
    /// MiB) — see `memtable::DEFAULT_MAX_SIZE_BYTES`'s own doc comment
    /// for why this, not "~64 MiB," is the followed value.
    pub memtable_max_size_bytes: usize,
    /// Operating brief §21/§43: bounded immutable-memtable backpressure.
    /// Once transient rather than permanent as of Phase 4B (the
    /// background flush thread drains this list — `PHASE4B_ADR.md`
    /// ADR-P4B-5), but still the one thing standing between sustained
    /// write load outpacing flush and unbounded memory growth.
    pub max_immutable_memtables: usize,
    /// `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.4 default: 4096 bytes.
    pub sstable_target_block_size: usize,
    /// `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.5 default: 10.
    pub bloom_bits_per_key: u32,
    /// `PHASE4B_ARCHITECTURE.md` §8: bounded flush retry before falling
    /// back to a longer periodic retry — not a hard failure limit (a
    /// flush is never abandoned, only its short-backoff phase is
    /// bounded).
    pub max_flush_retries: u32,
    /// `ADR-WE-SP-001` §8: once a flush's bounded fast-retry budget
    /// (`max_flush_retries`) is exhausted on an ENOSPC-classified
    /// failure specifically, this is the backoff used for every
    /// subsequent `STORAGE_PRESSURE` retry — replacing the flat,
    /// unconditional 2-second-forever cadence the pre-ADR code used for
    /// every kind of persistent flush failure
    /// (`PHASE5_ENOSPC_FAILURE_ANALYSIS.md` §5 traces the resulting
    /// 9,200-second retry storm to exactly that code path).
    pub storage_pressure_retry_interval: Duration,
    /// `RubixDB-LSM-Engine-Specification-v1.0.md` §5.1 / `ADR-
    /// COMPACTION-001` Decision 1/Decision 14: the live-SSTable count
    /// at which `LsmEngine::should_compact()` reports `true` — default
    /// 4, per the spec. Increment 1 defines this field and the
    /// deterministic `should_compact()`/`compact_once()` operations
    /// only; nothing in this codebase yet checks this field
    /// automatically (no background trigger exists — `ADR-COMPACTION-
    /// 001` Decision 14 explicitly defers that to a future increment).
    pub compaction_trigger_count: usize,
    /// `ADR-COMPACTION-001` Increment 2 amendment: whether `LsmEngine::
    /// open` spawns the background compaction worker thread at all.
    /// **Default `false`**, deliberately — see the amendment's own
    /// "Decision: default value" entry for the full reasoning. In
    /// short: this codebase's existing test suite (every phase up to
    /// and including the Read Engine's own certification) was written
    /// with no concept of compaction ever running, and `compaction_
    /// trigger_count`'s own spec-mandated default (4) is low enough
    /// that a great many pre-existing tests/fixtures across this crate
    /// -- not just this phase's own -- legitimately accumulate more
    /// than 4 live SSTables in the course of testing something else
    /// entirely. Defaulting automatic triggering *on* would silently
    /// start compacting out from under all of that already-certified,
    /// already-passing test surface the moment this field shipped,
    /// which is exactly the kind of "silently weaken an existing
    /// guarantee" this project's own standing principle forbids -- not
    /// a hypothetical risk: it was caught empirically, this increment,
    /// by two real test failures the first time `default_value = true`
    /// was tried (see the ADR amendment). `false` is the conservative,
    /// standard rollout posture for a new automatic subsystem: the full
    /// capability is implemented, tested, and available to any caller
    /// that explicitly opts in (`compaction_auto_trigger: true`) --
    /// this phase's own new tests do exactly that -- without silently
    /// changing behavior for every existing caller that hasn't. `compact
    /// _once()` remains directly, manually callable regardless of this
    /// flag's value, unaffected either way. Not a new trigger
    /// *criterion* (still purely the spec-mandated live-SSTable-count
    /// threshold) — a gate on whether that already-approved trigger is
    /// wired automatically at all.
    pub compaction_auto_trigger: bool,
}

impl Default for LsmConfig {
    fn default() -> Self {
        LsmConfig {
            memtable_max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
            max_immutable_memtables: 4,
            sstable_target_block_size: crate::sstable::format::DEFAULT_TARGET_BLOCK_SIZE,
            bloom_bits_per_key: crate::sstable::format::DEFAULT_BLOOM_BITS_PER_KEY,
            max_flush_retries: 3,
            storage_pressure_retry_interval: Duration::from_secs(5),
            compaction_trigger_count: 4,
            compaction_auto_trigger: false,
        }
    }
}

/// `ADR-WE-SP-001` §6: explicit storage-health state, orthogonal to the
/// pre-existing `CapacityExceeded` MemTable-freeze backpressure signal
/// (that contract — `[[project_rubixdb_capacity_contract]]` —  is
/// unchanged by this enum). `Healthy` is the only state in which a flush
/// failure gets purely the bounded fast-retry treatment; the other two
/// states exist so a *persistent* ENOSPC-classified failure (confirmed
/// by exhausting the fast-retry budget) cannot degrade into an unbounded
/// fixed-interval retry loop with no operator-visible signal — exactly
/// what the 2026-09-19 realistic soak found
/// (`PHASE5_ENOSPC_FAILURE_ANALYSIS.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StorageState {
    /// Normal operation. The last flush attempt (if any) succeeded, or
    /// no ENOSPC-classified failure has ever exhausted the fast-retry
    /// budget.
    Healthy = 0,
    /// A flush is retrying an ENOSPC-classified failure at the slower,
    /// configured `storage_pressure_retry_interval` cadence. Writes are
    /// still accepted normally (through the existing WAL/MemTable path,
    /// including the existing `CapacityExceeded` backpressure signal) —
    /// this state alone does not reject anything; it only replaces the
    /// old unbounded fast retry with a bounded, quieter one.
    StoragePressure = 1,
    /// The immutable-MemTable backlog reached `max_immutable_memtables`
    /// while a flush was already stuck in `StoragePressure` — the
    /// engine's configured safe-resource boundary. New writes now fail
    /// fast with `EngineError::StorageExhausted`, before any WAL append
    /// is attempted, rather than continuing to accumulate applied-but-
    /// unflushed state. Cleared back to `Healthy` the moment any flush
    /// attempt actually succeeds.
    StorageFull = 2,
}

impl StorageState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => StorageState::Healthy,
            1 => StorageState::StoragePressure,
            _ => StorageState::StorageFull,
        }
    }
}

/// Recovery observability (operating brief: "recovery duration,"
/// "recovery WAL records replayed," "recovery Manifest records
/// replayed") — computed once, during `LsmEngine::open`, and retained
/// for the engine's lifetime. Never influences any correctness
/// decision — purely diagnostic.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecoveryStats {
    /// Every WAL record `wal::replay_streaming` visited, whether
    /// applied, skipped by the checkpoint, or a replayed checkpoint
    /// marker — `wal_records_applied + wal_records_skipped_by_
    /// checkpoint + checkpoint_markers_replayed == wal_records_visited`
    /// always.
    pub wal_records_visited: u64,
    /// Put/Delete records actually applied to the freshly-reconstructed
    /// active MemTable (`seq` above the checkpoint).
    pub wal_records_applied: u64,
    /// Records with `seq <= checkpoint_seq` — already durably
    /// represented by the Manifest-authoritative SSTable set, correctly
    /// not replayed twice.
    pub wal_records_skipped_by_checkpoint: u64,
    /// `CHECKPOINT_MARKER` records visited above the checkpoint boundary
    /// — always a no-op for MemTable state (`RUBIC_MANIFEST_FORMAT_
    /// SPECIFICATION.md` §6), but real, durable WAL records in their own
    /// right (e.g. from a flush that wrote its marker but crashed before
    /// its `SET_CHECKPOINT` durably landed — `PHASE5_FAILURE_MODEL.md`
    /// §3 covers this exact case: safe, harmless, never data loss).
    pub checkpoint_markers_replayed: u64,
    /// Valid edits replayed from the `MANIFEST` file at startup.
    pub manifest_edits_replayed: u64,
    /// Wall-clock time for the complete `LsmEngine::open` call.
    pub recovery_duration: Duration,
}

/// A resolved read result — `Some(value)` for a live `Put`, `None` for
/// "not found" (no version at or before the query's `as_of_seq`, or the
/// newest visible version is a tombstone — operating brief §9/§14's own
/// collapse rule, applied once here across every source, not per-source).
pub type GetResult = Option<Vec<u8>>;

/// Message sent to the background flush thread (`PHASE4B_ADR.md`
/// ADR-P4B-5). Not `pub` — an internal implementation detail of the
/// flush pipeline, never part of `LsmEngine`'s public surface.
enum FlushMsg {
    Flush(Arc<MemTable>),
    Shutdown,
}

/// `ADR-COMPACTION-001` Increment 2 amendment: mirrors `FlushMsg`'s own
/// shape. `MaybeCompact` carries no payload deliberately -- it is a
/// pure "re-check `should_compact()` against current state" wake-up,
/// never a queued unit of work in its own right (the worker always
/// re-reads live state fresh when it wakes, never trusts a payload
/// that could already be stale by the time it's processed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactionMsg {
    MaybeCompact,
    Shutdown,
}

/// Deterministic flush-thread fault-injection points
/// (`PHASE_WRITE_ENGINE_TEST_PLAN.md`, closing the gap
/// `PHASE4B_FAILURE_MODEL.md`/`PHASE5_ADR.md` ADR-P5-5 named: the
/// existing SSTable/Manifest crash tests kill the whole process
/// externally, which never exercises `catch_unwind` at all — a killed
/// process doesn't unwind, it's simply gone. These points let a test
/// inject a real Rust panic *inside a live flush attempt* and verify the
/// in-process recovery path (`spawn_flush_thread`'s doc comment) instead.
/// Mirrors `execution::batch_coordinator::CoordinatorFaultPoint` and
/// `wal::group_commit::GroupCommitter::fsync_fault_hook`'s existing,
/// established pattern: an always-compiled enum plus an always-present
/// (not `#[cfg]`-gated, matching `fsync_fault_hook`'s own precedent)
/// `Mutex<Option<Hook>>`, so production builds pay one uncontended mutex
/// lock per flush-thread step and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlushFaultPoint {
    /// Before `sstable::write_from_memtable` — nothing for this flush is
    /// durable yet; a panic here must be indistinguishable from the
    /// flush thread never having started this attempt.
    BeforeSstableWrite,
    /// Immediately after the SSTable file and its `AddSstable` Manifest
    /// edit are both durable (`published` just set), before the table is
    /// installed into the live `sstables` list.
    AfterSstablePublish,
    /// Immediately after `pool.rotate()` returns.
    AfterRotate,
    /// Immediately after the WAL `CHECKPOINT_MARKER` is durable
    /// (`checkpoint_marker` just set), before `SET_CHECKPOINT` is
    /// appended to the Manifest — the exact step ordering
    /// `PHASE5_ADR.md`'s own real idempotent-retry bug was found at,
    /// under an external kill rather than a panic.
    AfterCheckpointMarker,
    /// Immediately after `SET_CHECKPOINT` is durable in the Manifest
    /// (`checkpoint_recorded` just set), before `checkpoint_seq` is
    /// published and `purge_before` is called.
    AfterSetCheckpoint,
}

type FlushFaultHook = Box<dyn Fn(FlushFaultPoint) + Send + Sync>;

/// Calls the installed flush fault hook, if any — a cheap no-op
/// (`Mutex` lock + `None` check) when nothing is installed, safe to call
/// unconditionally from every build including production.
fn fire_flush_fault_hook(hook: &Mutex<Option<FlushFaultHook>>, point: FlushFaultPoint) {
    let hook = hook.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(f) = hook.as_ref() {
        f(point);
    }
}

/// `ADR-WE-SP-001` §16: unlike `FlushFaultHook` (an observer with no
/// return value), this hook can *inject* a real `io::Error` at the exact
/// point the 2026-09-19 soak's flush attempts actually failed (just
/// before `sstable::write_from_memtable`) — so a test can drive the
/// storage-pressure state machine deterministically through a real
/// ENOSPC-shaped error, exercised by the same classification code
/// (`EngineError::is_storage_exhausted`) a real disk-full condition
/// would hit, without ever filling a real disk.
type FlushIoFaultHook = Box<dyn Fn() -> Option<io::Error> + Send + Sync>;

/// Same cheap-no-op-when-unset shape as `fire_flush_fault_hook`.
fn fire_flush_io_fault_hook(hook: &Mutex<Option<FlushIoFaultHook>>) -> Option<io::Error> {
    let hook = hook.lock().unwrap_or_else(|p| p.into_inner());
    hook.as_ref().and_then(|f| f())
}

/// `ADR-COMPACTION-001` Decision 15: deterministic fault-injection
/// points for `compact_once`, modeled directly on `FlushFaultPoint`
/// (same always-compiled-enum-plus-`Mutex<Option<Hook>>` shape, same
/// "production pays one uncontended mutex lock per step" cost). Named
/// to cover every crash window `PHASE_COMPACTION_ARCHITECTURE_REPORT.
/// md` §7 walked: `BeforeOutputWrite` (before output creation),
/// `BeforeManifestAdd` (output file fsync+rename already durable,
/// `AddSstable` not yet appended -- the "after output fsync/rename,
/// before Manifest ADD" window), `AfterManifestAdd` (`AddSstable`
/// durable, before any `RemoveSstable`), `DuringRemoveSequence` (fired
/// once per input, immediately after that input's own `RemoveSstable`
/// edit becomes durable -- covers "mid-REMOVE sequence"),
/// `AfterAllRemoves` (every `RemoveSstable` durable, before the live-
/// list splice and before any physical deletion attempt), and
/// `BeforePhysicalDelete` (splice done, about to attempt unlinking any
/// input whose `Arc` strong count already allows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompactionFaultPoint {
    BeforeOutputWrite,
    BeforeManifestAdd,
    AfterManifestAdd,
    DuringRemoveSequence,
    AfterAllRemoves,
    BeforePhysicalDelete,
}

/// `ADR-COMPACTION-001` Increment 2 amendment, §4: the single, smallest
/// synchronization primitive preventing two compactions (manual +
/// automatic, or any other overlap) from running concurrently --
/// acquired via `try_acquire` (a `compare_exchange` on a shared
/// `AtomicBool`, `None` if another compaction already holds it) at the
/// very start of `compact_once_impl`'s real work, released
/// automatically via `Drop` on every exit path (including `?`-
/// propagated errors) so a panicking or erroring compaction can never
/// leave the flag stuck `true`.
struct CompactionRunGuard<'a> {
    flag: &'a AtomicBool,
}

impl<'a> CompactionRunGuard<'a> {
    fn try_acquire(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| CompactionRunGuard { flag })
    }
}

impl Drop for CompactionRunGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

type CompactionFaultHook = Box<dyn Fn(CompactionFaultPoint) + Send + Sync>;

/// Same cheap-no-op-when-unset shape as `fire_flush_fault_hook`. Every
/// non-test caller of this function (and of `fire_compaction_io_fault_
/// hook`, `compact_once`, `should_compact`, `sweep_pending_compaction_
/// deletes`, `lock_sstables_write`, and the `pending_compaction_
/// deletes` field below) does not exist yet -- `ADR-COMPACTION-001`
/// Decision 14 explicitly defers automatic trigger wiring to a future
/// increment, so `compact_once` itself is, by design, only reachable
/// from tests this increment. Real, in-crate code, not unused or
/// speculative -- just not yet wired to a production caller, by
/// explicit ADR decision rather than oversight (mirrors this file's
/// own pre-existing `flush_delay_ms` field's identical `#[allow(dead_
/// code)]` justification). As of Increment 2, `compact_once_impl` (the
/// production compaction worker's own entry point) calls this
/// unconditionally -- no longer genuinely dead in non-test builds.
fn fire_compaction_fault_hook(
    hook: &Mutex<Option<CompactionFaultHook>>,
    point: CompactionFaultPoint,
) {
    let hook = hook.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(f) = hook.as_ref() {
        f(point);
    }
}

/// `ADR-COMPACTION-001` Decision 15: mirrors `FlushIoFaultHook`
/// exactly, but scoped to the one step Compaction's own output write
/// can genuinely fail at (mirrors the 2026-09-19 soak's own real
/// flush-side ENOSPC failure point) -- injected immediately before
/// `sstable::write_from_sorted_records` is called, letting a test
/// drive a real, classified `io::Error` through the same `EngineError::
/// is_storage_exhausted` classification path a genuine disk-full
/// condition would hit, without filling a real disk.
type CompactionIoFaultHook = Box<dyn Fn() -> Option<io::Error> + Send + Sync>;

/// Same cheap-no-op-when-unset shape as `fire_flush_io_fault_hook`.
fn fire_compaction_io_fault_hook(hook: &Mutex<Option<CompactionIoFaultHook>>) -> Option<io::Error> {
    let hook = hook.lock().unwrap_or_else(|p| p.into_inner());
    hook.as_ref().and_then(|f| f())
}

// ============================================================================
// Read Engine foundation (`ADR-RE-001`, Implementation Increment 1).
//
// Everything in this section is additive: no existing method's signature
// or behavior changes because of it. `get`/`get_as_of` gain exactly two
// kinds of new side effect -- `ReadStats` counter increments -- and
// nothing else; their control flow and return values are unchanged (see
// the comments at each insertion point below).
// ============================================================================

/// `ADR-RE-001` §2: tracks outstanding [`Snapshot`]s by sequence number,
/// as a multiset (`seq -> outstanding_count`) since two callers can
/// independently snapshot the same sequence. Grows only with the number
/// of *live* `Snapshot` handles, never with data volume -- dropping a
/// `Snapshot` always shrinks or removes its entry, never leaves it
/// behind. This is the mechanism a future Compaction phase will need
/// ("never remove a version still needed by the oldest live snapshot")
/// without this phase building Compaction itself.
#[derive(Default)]
struct SnapshotRegistry {
    counts: Mutex<BTreeMap<u64, u64>>,
}

impl SnapshotRegistry {
    fn acquire(&self, seq: u64) {
        let mut counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        *counts.entry(seq).or_insert(0) += 1;
    }

    /// Decrements `seq`'s outstanding count, removing the entry entirely
    /// once it reaches zero -- so `oldest()` never reports a sequence
    /// with zero live holders.
    fn release(&self, seq: u64) {
        let mut counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(count) = counts.get_mut(&seq) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&seq);
            }
        }
    }

    /// The lowest sequence with at least one live `Snapshot` still
    /// holding it, or `None` if no snapshot is currently outstanding.
    fn oldest(&self) -> Option<u64> {
        let counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        counts.keys().next().copied()
    }
}

/// `ADR-RE-001` §2: a caller-held point-in-time read watermark. Obtaining
/// one (`LsmEngine::snapshot()`) and passing its [`seq()`](Snapshot::seq)
/// to `get_as_of` (or, once implemented, `range_scan`) guarantees a
/// stable view "as of when I asked," independent of later writes --
/// exactly the semantics `get_as_of`'s existing bare `as_of_seq: u64`
/// parameter already provides, just with an explicit, registered
/// lifetime attached instead of a caller-supplied raw number. Registered
/// in the owning `LsmEngine`'s `snapshot_registry` for its entire
/// lifetime; deregistered automatically on `Drop`.
///
/// Deliberately does **not** implement `Clone` -- two independent
/// snapshots of the same `seq` are two independent registry entries
/// (`SnapshotRegistry::acquire` is called once per `LsmEngine::
/// snapshot()` call), not one shared handle; a caller wanting two must
/// call `snapshot()` twice (or explicitly reason about sharing one
/// `Arc<Snapshot>`, which remains possible without `Clone` on the
/// underlying type).
pub struct Snapshot {
    seq: u64,
    registry: Arc<SnapshotRegistry>,
}

impl Snapshot {
    /// The durable watermark this snapshot pins -- pass to `get_as_of`
    /// (and, once implemented, `range_scan`) for a stable, point-in-time
    /// read unaffected by writes that happen after this snapshot was
    /// taken.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.registry.release(self.seq);
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot").field("seq", &self.seq).finish()
    }
}

/// `ADR-RE-001` §12: read-path observability, mirroring the existing
/// `capacity_pressure_events`/`storage_pressure_events` convention
/// (cheap `Relaxed` atomics on `LsmEngine`, exposed via a snapshot-copy
/// accessor) rather than introducing a new observability pattern.
/// Definitions, precise (see `get_as_of`'s instrumentation below for
/// exactly where each is incremented):
/// - `read_hits`/`read_misses`: whether *some* source held a matching
///   `(key, seq)` entry at all (a found tombstone counts as a hit here,
///   even though the public `get`/`get_as_of` API still returns `Ok
///   (None)` for it -- this counter is about how much of the source
///   chain had to be searched, a different question from what the
///   caller ultimately sees).
/// - `bloom_negatives`/`blocks_read`: summed across every currently-live
///   `SsTable` at `read_stats()` call time (`SsTable::bloom_negative_
///   count`/`blocks_read` are each individually cumulative and, since no
///   SSTable is ever removed without Compaction, summing the live set
///   is exactly the lifetime total -- see `read_stats()`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadStats {
    pub read_requests: u64,
    pub read_hits: u64,
    pub read_misses: u64,
    pub bloom_negatives: u64,
    pub blocks_read: u64,
    pub sstables_consulted: u64,
}

/// The mutable (interior, via atomics) counters `ReadStats` is a
/// snapshot-copy of. `sstables_consulted` lives here (an `LsmEngine`-
/// level fact: how many tables *this engine's* reads have visited) --
/// `bloom_negatives`/`blocks_read` do not, deliberately: those are
/// per-`SsTable` facts (`ADR-RE-001` §12), summed from the live
/// `sstables` list at `read_stats()` time instead of duplicated here.
#[derive(Default)]
struct ReadStatCounters {
    requests: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    sstables_consulted: AtomicU64,
}

/// Increment 3 (production performance/resource/endurance validation):
/// cumulative, across-the-engine-lifetime Compaction health metrics,
/// mirroring `ReadStats`'s own "cheap atomic counters, snapshot-copied
/// on read" shape. Unlike `CompactionStats` (one instance per cycle,
/// already returned directly by `compact_once`/logged by the worker),
/// this is a running total plus the most recent cycle's own
/// `CompactionStats` — needed to answer "is compaction keeping up over
/// time" questions no single cycle's own stats can answer alone.
/// Purely additive observability: updated only on a *successful*
/// `compact_once_impl` cycle (mirroring the brief's own "each
/// successful automatic Compaction should produce observable..."
/// framing), never consulted by any correctness or trigger decision —
/// same non-load-bearing status `ReadStats` already has.
#[derive(Debug, Clone, Default)]
pub struct CompactionMetrics {
    pub cycles_completed: u64,
    pub input_sstables_total: u64,
    pub input_bytes_total: u64,
    pub output_bytes_total: u64,
    pub records_read_total: u64,
    pub records_retained_total: u64,
    pub records_dropped_total: u64,
    pub tombstones_dropped_total: u64,
    pub versions_dropped_total: u64,
    pub duration_total: Duration,
    pub duration_max: Duration,
    pub peak_temp_disk_bytes_max: u64,
    pub last_cycle: Option<CompactionStats>,
}

#[derive(Default)]
struct CompactionMetricCounters {
    cycles_completed: AtomicU64,
    input_sstables_total: AtomicU64,
    input_bytes_total: AtomicU64,
    output_bytes_total: AtomicU64,
    records_read_total: AtomicU64,
    records_retained_total: AtomicU64,
    records_dropped_total: AtomicU64,
    tombstones_dropped_total: AtomicU64,
    versions_dropped_total: AtomicU64,
    duration_total_nanos: AtomicU64,
    duration_max_nanos: AtomicU64,
    peak_temp_disk_bytes_max: AtomicU64,
    last_cycle: Mutex<Option<CompactionStats>>,
}

/// Records one successful cycle's `CompactionStats` into the running
/// totals. Called from `compact_once_impl` only on the `Ok(Some(..))`
/// path — a failed/deferred/no-op cycle contributes nothing (there is
/// no partial cycle to report; §11 of `ADR-COMPACTION-001`'s crash
/// protocol already establishes a failure leaves no observable partial
/// state, and this mirrors that for metrics).
fn record_compaction_cycle(counters: &CompactionMetricCounters, stats: &CompactionStats) {
    counters.cycles_completed.fetch_add(1, Ordering::Relaxed);
    counters
        .input_sstables_total
        .fetch_add(stats.input_sstable_count as u64, Ordering::Relaxed);
    counters
        .input_bytes_total
        .fetch_add(stats.input_bytes, Ordering::Relaxed);
    counters
        .output_bytes_total
        .fetch_add(stats.output_bytes, Ordering::Relaxed);
    counters
        .records_read_total
        .fetch_add(stats.records_read, Ordering::Relaxed);
    counters
        .records_retained_total
        .fetch_add(stats.records_retained, Ordering::Relaxed);
    counters
        .records_dropped_total
        .fetch_add(stats.records_dropped, Ordering::Relaxed);
    counters
        .tombstones_dropped_total
        .fetch_add(stats.tombstones_dropped, Ordering::Relaxed);
    counters
        .versions_dropped_total
        .fetch_add(stats.versions_dropped, Ordering::Relaxed);
    let dur_nanos = stats.duration.as_nanos().min(u128::from(u64::MAX)) as u64;
    counters
        .duration_total_nanos
        .fetch_add(dur_nanos, Ordering::Relaxed);
    counters
        .duration_max_nanos
        .fetch_max(dur_nanos, Ordering::Relaxed);
    counters
        .peak_temp_disk_bytes_max
        .fetch_max(stats.peak_temp_disk_bytes, Ordering::Relaxed);
    *counters
        .last_cycle
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(stats.clone());
}

/// `ADR-RE-001` §1/§5: the `range_scan` consistency mechanism --
/// captures a stable set of source references once, at construction
/// time, so the k-way merge (`RangeScanIter`) can run lock-free against
/// an unchanging view for the scan's whole duration. Point lookups
/// (`get`/`get_as_of`) do **not** use this -- they keep the existing,
/// separately-safe sequential-lock pattern (`ADR-RE-001` §1's
/// "Alternatives considered").
///
/// Deliberately does not clone MemTable/SSTable *contents*:
/// `immutables`/`sstables` below are `Arc` clones (a refcount bump each,
/// never a data copy -- per `ADR-RE-001` §12, this must never duplicate
/// a `BloomFilter`/`Vec<IndexEntry>`, and it does not: no new `BloomFilter`
/// or index structure is constructed anywhere in `capture_read_view`).
/// `active_range` materializes only the entries already inside the
/// requested `[start, end)`, never the whole active MemTable.
pub(crate) struct ReadView {
    /// Entries from the active MemTable already inside the requested
    /// range, *not yet version-resolved* -- mirrors `MemTable::range`'s
    /// own "all versions in range, caller resolves" contract exactly
    /// (`ADR-RE-001` §5's explicit discrepancy note: `range`/
    /// `range_scan_raw` are lower-level than `get`/`get_as_of`).
    active_range: Vec<((Vec<u8>, u64), MemtableValue)>,
    /// Newest-first -- same ordering convention as `LsmEngine.immutables`.
    immutables: Vec<Arc<MemTable>>,
    /// Newest-first -- same ordering convention as `LsmEngine.sstables`.
    sstables: Vec<Arc<SsTable>>,
}

/// `ADR-RE-001` §6/§15: `std::collections::BTreeMap::range` **panics**
/// (rather than returning an empty iterator) when `start > end`, or
/// when `start == end` and both bounds are `Excluded` -- both are
/// mathematically empty intervals, not errors, so every entry point
/// that could reach a `BTreeMap::range` call (`capture_read_view`'s own
/// `active.range(..)`, transitively `MemTable::range` for immutables)
/// must detect and short-circuit to an empty result *before* ever
/// constructing such a range, rather than let the panic happen. Caught
/// by actually running the `start > end` and `Excluded(x)..Excluded(x)`
/// cases in `range_scan_empty_cases` before trusting this code -- the
/// first version of this increment panicked on exactly this input.
fn range_is_definitely_empty(start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    match (start, end) {
        (Bound::Unbounded, _) | (_, Bound::Unbounded) => false,
        (Bound::Included(s), Bound::Included(e)) => s > e,
        (Bound::Included(s), Bound::Excluded(e))
        | (Bound::Excluded(s), Bound::Included(e))
        | (Bound::Excluded(s), Bound::Excluded(e)) => s >= e,
    }
}

fn to_owned_bound(b: Bound<&[u8]>) -> Bound<Vec<u8>> {
    match b {
        Bound::Included(k) => Bound::Included(k.to_vec()),
        Bound::Excluded(k) => Bound::Excluded(k.to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn bound_as_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(k) => Bound::Included(k.as_slice()),
        Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn record_value_to_memtable_value(v: RecordValue) -> MemtableValue {
    match v {
        RecordValue::Put(bytes) => MemtableValue::Put(bytes),
        RecordValue::Tombstone => MemtableValue::Tombstone,
    }
}

/// `ADR-RE-001` §4: which `ReadView` source a `HeapEntry` came from --
/// carries enough information for `RangeScanIter` to know which cursor
/// to advance once that entry's key has been resolved (won or lost).
/// The `usize` payloads are indices into `ReadView.immutables`/
/// `ReadView.sstables`, not recency ranks (recency is tracked
/// separately on `HeapEntry` itself, since it depends on where in the
/// newest-first list the index falls, not the index value itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeSource {
    Active,
    Immutable(usize),
    SsTable(usize),
}

/// One source's contribution to the k-way merge at its current
/// position: the next distinct key it holds (at or after that source's
/// own resume point) and *every* version of that key from this source
/// (`ADR-RE-001` §3: source iterators expose unresolved, all-version
/// streams -- version resolution is this merge layer's job, never
/// assumed already done).
struct HeapEntry {
    key: Vec<u8>,
    /// `ADR-RE-001` §4: `0` = active, `1..=immutables.len()` = immutable
    /// MemTables newest-first, then live SSTables newest-first. Lower
    /// number = more recent = resolved first when multiple sources tie
    /// on `key`.
    recency: usize,
    source: RangeSource,
    versions: Vec<(u64, MemtableValue)>,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.recency == other.recency
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    /// Primary: key ascending. Secondary: recency ascending (`ADR-RE-001`
    /// §4's exact heap order). Used inside a `BinaryHeap<Reverse<
    /// HeapEntry>>` so the heap's own max-heap behavior pops the
    /// smallest `(key, recency)` pair first.
    fn cmp(&self, other: &Self) -> CmpOrdering {
        (&self.key, self.recency).cmp(&(&other.key, other.recency))
    }
}

/// `ADR-RE-001` §3/§4/§6/§7: the `range_scan`/`range` return type -- a
/// lazy, ordered, bounded-memory, single-logical-value-per-key iterator
/// over a [`ReadView`] captured exactly once, at construction. After
/// construction, iteration touches only the captured `ReadView` and
/// each live `SsTable`'s own already-open `File` -- never the live
/// engine's `immutables`/`sstables` lists, never the filesystem sweep,
/// never `ManifestState`.
///
/// **Why immutable-MemTable source iterators are re-queried per key**:
/// `MemTable::range()` returns an iterator borrowing from the `MemTable`
/// it was called on. Storing such a borrowing iterator in the *same*
/// struct as the `Arc<MemTable>` it borrows from is a self-referential
/// struct Rust's borrow checker cannot express without `unsafe` or an
/// external crate (e.g. `ouroboros`/`self_cell`) -- neither is warranted
/// here, and `MemTable::range` is a cheap, in-memory `BTreeMap::range`
/// call (O(log n) tree descent, no I/O, no block decode), so re-issuing
/// it once per distinct key was never the measured cost (`PHASE_READ_
/// ENGINE_RESOURCE_INVESTIGATION.md` traced the entire soak-observed
/// range-latency blowup to the SSTable side, never the MemTable side)
/// -- left exactly as it was, per `ADR-RE-002`'s own explicit "do not
/// rewrite MemTable logic unnecessarily" instruction. Each immutable
/// source still tracks only its own resume point (an owned `Bound<Vec<
/// u8>>`); every peek makes one fresh, short-lived call to `range`
/// (dropped at the end of that one call), pulls every version of
/// exactly the next distinct key, and advances the resume point.
///
/// **SSTable sources: `ADR-RE-002` Option A, implemented this
/// increment.** Until Increment 6, each `SsTable` source used the same
/// resume-point-plus-fresh-call pattern as immutables above, via
/// `SsTable::range_scan_raw<'a>(&'a self, ..)` -- which meant re-running
/// that call's own block-locating binary search, and (far more costly,
/// per `PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md` §4's traced root
/// cause) re-reading and re-decoding a data block from scratch every
/// time a key drawn from that source's remaining decoded records had
/// already been consumed by the previous call -- once per distinct key
/// a source contributed, not once per block. On this project's own
/// realistic (small-cardinality, heavily-overwritten) endurance
/// workload, that meant re-peeking essentially every live SSTable once
/// per yielded key: O(distinct keys yielded x live SSTable count),
/// measured growing `range_large` p50 from 1.15ms to 43.1 seconds over
/// a 4-hour soak (614 SSTables). Each `sstable_cursors` entry below now
/// holds a persistent [`SsTableRangeCursor`] (`src/sstable/reader.rs`)
/// -- constructed once per source at scan start and driven forward
/// with ordinary `Iterator::next()`/`Peekable::peek()` calls for the
/// scan's entire remaining lifetime, never reconstructed. Because
/// `SsTableRangeCursor` owns its own `Arc<SsTable>` clone (a refcount
/// bump, not a data copy -- the same pattern `ReadView` already uses)
/// rather than borrowing `&'a SsTable`, storing it here is not
/// self-referential: no `unsafe`, no `ouroboros`/`self_cell`, no new
/// dependency. The k-way merge algorithm itself (`refill`/`next` below)
/// is unchanged -- only how an SSTable source produces its next record
/// changed.
pub struct RangeScanIter {
    read_view: ReadView,
    as_of_seq: u64,
    end: Bound<Vec<u8>>,
    /// Position into `read_view.active_range` -- already fully
    /// materialized and range-filtered at capture time, so no bound
    /// re-query is ever needed for this source.
    active_position: usize,
    /// `None` once that immutable is known exhausted (no re-query is
    /// attempted again for it). Unchanged by `ADR-RE-002` -- see this
    /// struct's own doc comment for why immutables keep this design.
    immutable_next_start: Vec<Option<Bound<Vec<u8>>>>,
    /// `ADR-RE-002` Option A: one persistent, owned-`Arc` cursor per
    /// live SSTable source, constructed once (`new`, at scan start) and
    /// driven forward via `Peekable::peek`/`next` for the rest of the
    /// scan -- never reconstructed per key. `None` once that source is
    /// known exhausted (its cursor is dropped at that point, releasing
    /// its `Arc<SsTable>` clone and decoded-block buffer immediately,
    /// rather than waiting for the whole scan to finish -- see §6/§17
    /// of `PROGRESS.md`'s Increment 6 entry for the resource-lifetime
    /// verification this enables).
    sstable_cursors: Vec<Option<std::iter::Peekable<SsTableRangeCursor>>>,
    heap: BinaryHeap<std::cmp::Reverse<HeapEntry>>,
    heap_initialized: bool,
    read_stats: Arc<ReadStatCounters>,
    /// Sticky once a source yields an `Err` -- `ADR-RE-001` §7: stop on
    /// first corruption/I/O error, never continue, never yield anything
    /// after the error.
    errored: bool,
}

impl RangeScanIter {
    fn new(
        read_view: ReadView,
        start: Bound<Vec<u8>>,
        end: Bound<Vec<u8>>,
        as_of_seq: u64,
        read_stats: Arc<ReadStatCounters>,
    ) -> Self {
        let immutable_next_start = vec![Some(start.clone()); read_view.immutables.len()];
        // `ADR-RE-002` Option A: construct every live SSTable source's
        // persistent cursor up front -- cheap (one in-memory
        // `partition_point` binary search per source, zero I/O; the
        // first real block read happens lazily, on the first `next()`
        // pulled from a given cursor, exactly as before) and it removes
        // the need to track a separate resume-point `Bound` per source,
        // since the cursor's own internal block index/decoded-record
        // position already *is* the resume point.
        let sstable_cursors: Vec<Option<std::iter::Peekable<SsTableRangeCursor>>> = read_view
            .sstables
            .iter()
            .map(|table| {
                Some(
                    SsTable::range_scan_cursor(Arc::clone(table), start.clone(), end.clone())
                        .peekable(),
                )
            })
            .collect();
        RangeScanIter {
            read_view,
            as_of_seq,
            end,
            active_position: 0,
            immutable_next_start,
            sstable_cursors,
            heap: BinaryHeap::new(),
            heap_initialized: false,
            read_stats,
            errored: false,
        }
    }

    /// Peeks the next distinct key from the pre-materialized
    /// `active_range` (already sorted `(key, seq)` ascending, already
    /// filtered to the requested range at `capture_read_view` time) --
    /// no I/O, no re-query, just a linear scan forward from the last
    /// position.
    fn peek_active(&mut self) -> Option<HeapEntry> {
        let range = &self.read_view.active_range;
        if self.active_position >= range.len() {
            return None;
        }
        let key = range[self.active_position].0 .0.clone();
        let mut versions = Vec::new();
        while self.active_position < range.len() && range[self.active_position].0 .0 == key {
            let ((_, seq), value) = &range[self.active_position];
            versions.push((*seq, value.clone()));
            self.active_position += 1;
        }
        Some(HeapEntry {
            key,
            recency: 0,
            source: RangeSource::Active,
            versions,
        })
    }

    fn peek_immutable(&mut self, idx: usize, recency: usize) -> Option<HeapEntry> {
        let next_start = self.immutable_next_start[idx].clone()?;
        let table = &self.read_view.immutables[idx];
        let mut iter = table
            .range(bound_as_ref(&next_start), bound_as_ref(&self.end))
            .peekable();
        let (first_tuple, first_value) = iter.next()?;
        let key = first_tuple.0.clone();
        let mut versions = vec![(first_tuple.1, first_value.clone())];
        while iter.peek().is_some_and(|(t, _)| t.0 == key) {
            let (tuple, value) = iter.next().expect("just confirmed present by peek");
            versions.push((tuple.1, value.clone()));
        }
        drop(iter);
        self.immutable_next_start[idx] = Some(Bound::Excluded(key.clone()));
        Some(HeapEntry {
            key,
            recency,
            source: RangeSource::Immutable(idx),
            versions,
        })
    }

    /// `ADR-RE-002` Option A: pulls the next distinct key's full version
    /// group from this source's *persistent* cursor -- `Peekable::peek`/
    /// `next` on the same [`SsTableRangeCursor`] constructed once in
    /// `new`, never a freshly reconstructed one. No binary search, no
    /// discarded/re-read block: the cursor's own `next_block_idx`/
    /// `current` (decoded-block) state already carries forward from
    /// wherever the previous call left off. Logically identical to the
    /// pre-Increment-6 behavior otherwise -- same grouping loop, same
    /// fail-closed-on-`Err` contract (still must not `break` past a
    /// corrupted record and return an incomplete `versions` as if
    /// complete: the corrupted record is still logically part of this
    /// key's version group).
    fn peek_sstable(&mut self, idx: usize, recency: usize) -> Result<Option<HeapEntry>> {
        let Some(cursor) = self.sstable_cursors[idx].as_mut() else {
            return Ok(None);
        };
        let (key, first_seq, first_value) = match cursor.next() {
            Some(Ok(v)) => v,
            Some(Err(e)) => return Err(e),
            None => {
                // Exhausted -- drop the cursor now rather than at scan
                // end, releasing its `Arc<SsTable>` clone and decoded-
                // block buffer immediately (§6/§17 resource-lifetime
                // requirement: a completed source must not stay
                // reachable/retained for the rest of the scan).
                self.sstable_cursors[idx] = None;
                return Ok(None);
            }
        };
        let mut versions = vec![(first_seq, record_value_to_memtable_value(first_value))];
        loop {
            let cursor = self.sstable_cursors[idx]
                .as_mut()
                .expect("cursor just yielded Some(..) above, not yet cleared");
            match cursor.peek() {
                Some(Ok((k, _, _))) if *k == key => {}
                // Propagate immediately -- must NOT `break` past this
                // and return the already-collected (incomplete)
                // `versions` as if they were a complete, successful
                // result: the corrupted record is still logically part
                // of *this* key's version group, so this exact call
                // must fail, not a later one (`ADR-RE-001` §7).
                Some(Err(_)) => match cursor.next() {
                    Some(Err(e)) => return Err(e),
                    _ => unreachable!("peek() just confirmed Err present"),
                },
                _ => break,
            }
            match cursor.next() {
                Some(Ok((_, seq, value))) => {
                    versions.push((seq, record_value_to_memtable_value(value)))
                }
                _ => unreachable!("just confirmed matching key present by peek"),
            }
        }
        Ok(Some(HeapEntry {
            key,
            recency,
            source: RangeSource::SsTable(idx),
            versions,
        }))
    }

    /// Peeks and pushes the current position of every source named in
    /// `only` (or every non-exhausted source, if `only` is `None` --
    /// used once, to seed the heap on the very first `next()` call).
    /// Subsequent calls pass `Some(&sources_just_advanced)`, matching
    /// the standard k-way merge pattern: a source's heap entry stays
    /// valid and untouched until that specific source is the one that
    /// gets consumed and must be re-peeked.
    fn refill(&mut self, only: Option<&[RangeSource]>) -> Result<()> {
        let want = |s: RangeSource| only.is_none_or(|list| list.contains(&s));

        if want(RangeSource::Active) {
            if let Some(entry) = self.peek_active() {
                self.heap.push(std::cmp::Reverse(entry));
            }
        }
        for idx in 0..self.read_view.immutables.len() {
            if want(RangeSource::Immutable(idx)) {
                // Recency `1..=immutables.len()`, newest immutable = 1,
                // matching `ADR-RE-001` §4 and `LsmEngine.immutables`'
                // own newest-first ordering convention exactly.
                if let Some(entry) = self.peek_immutable(idx, 1 + idx) {
                    self.heap.push(std::cmp::Reverse(entry));
                }
            }
        }
        let sstable_recency_base = 1 + self.read_view.immutables.len();
        for idx in 0..self.read_view.sstables.len() {
            if want(RangeSource::SsTable(idx)) {
                if let Some(entry) = self.peek_sstable(idx, sstable_recency_base + idx)? {
                    self.heap.push(std::cmp::Reverse(entry));
                }
            }
        }
        Ok(())
    }
}

impl Iterator for RangeScanIter {
    /// One resolved, visible `Put` per logical key -- never a
    /// tombstone, never a duplicate key, never a version with `seq >
    /// as_of_seq` (`ADR-RE-001` §4/§6/§7).
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored {
            return None;
        }
        if !self.heap_initialized {
            self.heap_initialized = true;
            // `ADR-RE-002`/`PHASE_READ_ENGINE_PERFORMANCE.md`'s revised
            // `sstables_consulted` semantics (Increment 6): counted once
            // per live SSTable this scan's `ReadView` captured -- i.e.
            // once per table actually consulted for this one logical
            // range-scan operation -- not once per distinct key drawn
            // from a source, which is what the pre-Increment-6
            // implementation (necessarily, since every key draw was a
            // fresh `range_scan_raw` call) counted instead. This matches
            // point lookups' own already-established convention exactly
            // ("once per table checked... whether or not that check was
            // a bloom-negative", `PHASE_READ_ENGINE_PERFORMANCE.md`) --
            // an attempted-consultation count, not a result count -- and
            // is counted here, at the single point where every source's
            // persistent cursor is known to exist, rather than
            // scattered across `peek_sstable` calls that may now number
            // fewer than one per source (a source can be asked for its
            // next key zero times if the merge never needs it) or more
            // than one (repeated draws from the *same*, already-open
            // cursor no longer represent a new consultation).
            self.read_stats
                .sstables_consulted
                .fetch_add(self.read_view.sstables.len() as u64, Ordering::Relaxed);
            if let Err(e) = self.refill(None) {
                self.errored = true;
                return Some(Err(e));
            }
        }
        loop {
            let std::cmp::Reverse(first) = self.heap.pop()?;
            let winning_key = first.key.clone();
            let mut group = vec![first];
            while let Some(std::cmp::Reverse(top)) = self.heap.peek() {
                if top.key != winning_key {
                    break;
                }
                let std::cmp::Reverse(next_entry) =
                    self.heap.pop().expect("just confirmed present by peek");
                group.push(next_entry);
            }
            // `group` is already sorted by recency ascending: every
            // member shares `winning_key`, and the heap's own `Ord`
            // sorts secondarily by recency, so pop order among ties
            // *is* recency order.

            // `ADR-RE-001` §4/§5: within each source, take the highest
            // `seq <= as_of_seq`; among sources holding this key, the
            // first (most recent) one with *any* visible version wins
            // -- structurally equivalent to "highest seq across all
            // sources" (never resurrects an older value hidden by a
            // newer tombstone) because this project's recency ordering
            // already guarantees a newer source's versions are always
            // newer than an older source's, so nothing an older source
            // holds could ever outrank a newer source's own visible
            // answer.
            let mut winner: Option<MemtableValue> = None;
            for entry in &group {
                let best = entry
                    .versions
                    .iter()
                    .filter(|(seq, _)| *seq <= self.as_of_seq)
                    .max_by_key(|(seq, _)| *seq);
                if let Some((_, value)) = best {
                    winner = Some(value.clone());
                    break;
                }
            }

            let sources: Vec<RangeSource> = group.iter().map(|e| e.source).collect();
            if let Err(e) = self.refill(Some(&sources)) {
                self.errored = true;
                return Some(Err(e));
            }

            match winner {
                Some(MemtableValue::Put(v)) => {
                    self.read_stats.hits.fetch_add(1, Ordering::Relaxed);
                    return Some(Ok((winning_key, v)));
                }
                // Tombstone: suppress, per `ADR-RE-001` §6 -- never
                // yielded, never a sentinel, never resurrects an older
                // source's value for this same key (the loop above
                // already stopped at the first, newest, visible answer).
                // None: no source had any version of this key visible
                // at `as_of_seq` -- also nothing to yield.
                Some(MemtableValue::Tombstone) | None => continue,
            }
        }
    }
}

pub struct LsmEngine {
    pool: Arc<BatchCoordinatorPool>,
    active: RwLock<MemTable>,
    immutables: Arc<RwLock<VecDeque<Arc<MemTable>>>>,
    /// Newest-first, per `RubixDB-LSM-Engine-Specification-v1.0.md`
    /// §4.1's own `sstables: RwLock<Vec<Arc<SSTable>>>` convention.
    /// Populated at `open()` by `sstable::discover` and appended to (at
    /// index 0) by the background flush thread after each successful
    /// publish (`PHASE4B_ARCHITECTURE.md` §4-§5).
    sstables: Arc<RwLock<Vec<Arc<SsTable>>>>,
    next_sstable_id: Arc<AtomicU64>,
    sstables_dir: PathBuf,
    /// The Manifest — authoritative record of live SSTables and the
    /// durable checkpoint boundary (`RUBIC_MANIFEST_FORMAT_
    /// SPECIFICATION.md`, `PHASE5_MANIFEST_ARCHITECTURE.md`). Shared
    /// with the background flush thread, the only other appender.
    manifest: Arc<Mutex<Manifest>>,
    /// The current durable checkpoint (`flushed_through_seq`), `0` if
    /// none has ever been recorded — mirrors, does not replace, the
    /// Manifest's own on-disk `SET_CHECKPOINT` state (Section 5 of the
    /// architecture doc: belt-and-suspenders monotonicity, plus cheap
    /// observability without re-reading the Manifest file).
    checkpoint_seq: Arc<AtomicU64>,
    recovery_stats: RecoveryStats,
    flush_sender: mpsc::Sender<FlushMsg>,
    flush_handle: Mutex<Option<JoinHandle<()>>>,
    /// Checked by the flush thread between bounded-retry backoff sleeps
    /// (`PHASE4B_ARCHITECTURE.md` §8) so a persistently-failing flush
    /// cannot block `shutdown` indefinitely — set once, by `shutdown`,
    /// never cleared.
    flush_stop: Arc<std::sync::atomic::AtomicBool>,
    /// Test-only artificial per-attempt flush delay, milliseconds
    /// (`set_flush_delay_for_test`) — lets tests deterministically widen
    /// the window in which an `ImmutableMemTable` remains un-flushed,
    /// instead of depending on incidental relative timing between the
    /// write path and the background flush thread's own (real) disk I/O.
    /// Zero (the always-on production default) has no effect. The
    /// `Arc`'s value is always read by the background flush thread
    /// (`spawn_flush_thread`, unconditionally, every build); this
    /// `self`-owned clone of it is only ever *written* through, via
    /// `set_flush_delay_for_test`, which is itself compiled only under
    /// `cfg(test)`/`test-util` — hence the field-level `dead_code`
    /// suppression below rather than a genuine unused field.
    #[allow(dead_code)]
    flush_delay_ms: Arc<AtomicU64>,
    /// Number of times `freeze_locked` skipped a freeze because
    /// `immutables` was already at `max_immutable_memtables` — the
    /// soft-success capacity-pressure signal (see `freeze_locked`'s doc
    /// comment). Every write that triggered this still returned `Ok`;
    /// this counter is purely observational, never consulted by any
    /// correctness decision.
    capacity_pressure_events: AtomicU64,
    /// See `FlushFaultPoint`/`fire_flush_fault_hook`. Shared with the
    /// background flush thread via the `Arc` `open()` clones into
    /// `spawn_flush_thread`; this copy is what `install_flush_fault_hook`/
    /// `clear_flush_fault_hook` write through.
    flush_fault_hook: Arc<Mutex<Option<FlushFaultHook>>>,
    /// `ADR-WE-SP-001` §6. Shared with the background flush thread (the
    /// sole writer on a successful/failed flush) and read by `put`/
    /// `delete` (`reject_if_storage_full`) and `freeze_locked` (the
    /// `StoragePressure` -> `StorageFull` promotion). `AtomicU8` rather
    /// than a `Mutex<StorageState>` so the hot `put`/`delete` path pays
    /// one relaxed-ish atomic load, not a lock.
    storage_state: Arc<AtomicU8>,
    /// Count of ENOSPC-classified flush failures observed (`ADR-WE-SP-001`
    /// §14) — distinct from `capacity_pressure_events`, which counts a
    /// different failure mode (MemTable-freeze backpressure).
    storage_pressure_events: Arc<AtomicU64>,
    /// Test/observability-only: incremented once per successful flush
    /// job, strictly after that job's storage_state swap (see
    /// `spawn_flush_thread`'s own comment at the increment site). Lets
    /// a test wait for a flush job to be fully settled, not merely
    /// removed from `immutables` -- see `flush_completions()`. `#[allow
    /// (dead_code)]`: only `cfg(test)` code (`flush_completions()`
    /// itself, and its own callers in `tests.rs`) ever reads it, so a
    /// non-test `cargo build`/`cargo check` sees no reader -- same
    /// convention as `pending_compaction_deletes`/`compaction_running`.
    #[allow(dead_code)]
    flush_completions: Arc<AtomicU64>,
    /// See `FlushIoFaultHook`/`fire_flush_io_fault_hook`. Test-only in
    /// practice (nothing in production ever calls
    /// `install_flush_io_fault_hook`), always compiled — same
    /// uncontended-mutex-lock-and-`None`-check convention as
    /// `flush_fault_hook`.
    flush_io_fault_hook: Arc<Mutex<Option<FlushIoFaultHook>>>,
    /// `ADR-RE-001` §2. `Arc`-wrapped even though nothing else currently
    /// shares it, matching `Snapshot`'s own need to hold a clone of it
    /// for its `Drop` impl to reach back into.
    snapshot_registry: Arc<SnapshotRegistry>,
    /// `ADR-RE-001` §12. See `ReadStatCounters`'s own doc comment for
    /// exactly which counters live here vs. on `SsTable`. `Arc`-wrapped
    /// (Increment 2) so a detached `RangeScanIter` -- which outlives the
    /// `&self` borrow that created it, per `ADR-RE-001` §1's `ReadView`
    /// design -- can still update `sstables_consulted`/`hits` as it's
    /// driven, without holding a reference back to the engine itself.
    read_stats: Arc<ReadStatCounters>,
    config: LsmConfig,
    /// See `CompactionFaultHook`/`fire_compaction_fault_hook`. Test-only
    /// in practice, always compiled — same convention as `flush_fault_
    /// hook`.
    compaction_fault_hook: Arc<Mutex<Option<CompactionFaultHook>>>,
    /// See `CompactionIoFaultHook`/`fire_compaction_io_fault_hook`.
    compaction_io_fault_hook: Arc<Mutex<Option<CompactionIoFaultHook>>>,
    /// `ADR-COMPACTION-001` Decision 9: input SSTables already removed
    /// from the Manifest and from the live `sstables` list by a
    /// completed `compact_once` call, but whose `Arc` strong count was
    /// still `> 1` (an in-flight reader) at the time — retried, never
    /// blocked on, by the next `compact_once` call's own opening sweep
    /// (`sweep_pending_compaction_deletes`). `Arc`-wrapped (Increment 2)
    /// so the background compaction worker thread can share it with
    /// `&self`'s own manual `compact_once` entry point. Note: the
    /// automatic path (the background worker) holds its *own*
    /// independent clone of this same `Arc`, taken directly from the
    /// local variable at `open()` time -- it never reads this field
    /// back out of `self`, so (like `compact_once`/`should_compact`
    /// themselves) this field's only *reader* is the still-non-
    /// production-called manual method, hence still flagged dead-code
    /// in a non-test build despite being real, live, shared state.
    #[allow(dead_code)]
    pending_compaction_deletes: Arc<Mutex<Vec<Arc<SsTable>>>>,
    /// `ADR-COMPACTION-001` Increment 2, §4: the smallest primitive
    /// that prevents two compactions (manual + automatic, or two
    /// automatic attempts somehow overlapping) from running at once --
    /// a single `AtomicBool` guard `compact_once_impl` acquires via
    /// `compare_exchange` at its own start and releases (via `Drop`)
    /// at every exit path, never a global engine lock. Same "own
    /// independent worker-thread clone, field itself only read by the
    /// still-non-production-called manual method" note as `pending_
    /// compaction_deletes` above applies here too.
    #[allow(dead_code)]
    compaction_running: Arc<AtomicBool>,
    /// Notifies the background compaction worker (if one is running --
    /// see `compaction_auto_trigger`) that new state exists worth
    /// re-checking `should_compact()` against. A bounded, capacity-1
    /// `sync_channel`: `try_send` coalesces redundant notifications
    /// (a channel already holding one unconsumed `MaybeCompact` simply
    /// drops a second one -- the worker will re-check state fresh
    /// either way) rather than letting them queue unboundedly. Sending
    /// to a disconnected receiver (no worker spawned, `compaction_
    /// auto_trigger=false`) is a harmless, ignored `Err`.
    compaction_sender: mpsc::SyncSender<CompactionMsg>,
    /// Mirrors `flush_handle`'s own shape exactly -- `None` when no
    /// worker was spawned (`compaction_auto_trigger=false`, or a raw-
    /// constructed test engine), in which case `shutdown()`'s join step
    /// is a no-op, same precedent as `flush_handle`.
    compaction_handle: Mutex<Option<JoinHandle<()>>>,
    /// Mirrors `flush_stop`'s own shape and purpose exactly.
    compaction_stop: Arc<AtomicBool>,
    /// Increment 3: cumulative Compaction health metrics, updated by
    /// `compact_once_impl` on every successful cycle regardless of
    /// which caller (manual or the automatic worker) triggered it —
    /// see `CompactionMetrics`/`record_compaction_cycle`.
    compaction_metrics: Arc<CompactionMetricCounters>,
}

impl LsmEngine {
    /// Opens (or creates) the WAL directory at `dir`. Startup ordering
    /// (`PHASE5_MANIFEST_ARCHITECTURE.md` §4 has the full derivation):
    ///
    /// 1. `manifest::replay_readonly` (shared lock, read-only) — obtains
    ///    the checkpoint boundary needed to filter WAL replay, *before*
    ///    any exclusive lock is taken (preserves the same same-process
    ///    lock-ordering constraint `PHASE4A_ADR.md` ADR-P4A-3 already
    ///    established for `wal::replay_streaming`).
    /// 2. `wal::replay_streaming` (shared lock, unchanged bounded-memory
    ///    design) — now discards any record already covered by the
    ///    checkpoint (LSM Engine Spec §7.1 step 5), entirely inside this
    ///    call's own closure; no signature change to the WAL module.
    /// 3. `FileWal::open_for_recovery` (exclusive lock, unchanged).
    /// 4. `Manifest::open_after_exclusive_lock` (re-replay under
    ///    exclusive protection) + the SSTable-directory reconciliation
    ///    sweep — the only write-capable part of Manifest recovery,
    ///    which is why it must wait for the exclusive lock.
    pub fn open(
        dir: &Path,
        wal_config: WalConfig,
        pool_config: BatchCoordinatorConfig,
        lsm_config: LsmConfig,
    ) -> Result<Self> {
        let recovery_started = std::time::Instant::now();
        let mut active = MemTable::new(lsm_config.memtable_max_size_bytes);

        let readonly_manifest = manifest::replay_readonly(dir)?;
        let checkpoint_at_replay = readonly_manifest.state.checkpoint_seq();

        let mut wal_records_visited: u64 = 0;
        let mut wal_records_applied: u64 = 0;
        let mut wal_records_skipped_by_checkpoint: u64 = 0;
        let mut checkpoint_markers_replayed: u64 = 0;
        let _summary = wal::replay_streaming(dir, &wal_config, |seq, op| {
            wal_records_visited += 1;
            if seq <= checkpoint_at_replay {
                wal_records_skipped_by_checkpoint += 1;
                return Ok(());
            }
            match op {
                WalOp::CheckpointMarker { .. } => checkpoint_markers_replayed += 1,
                _ => {
                    apply_wal_op(&mut active, seq, op);
                    wal_records_applied += 1;
                }
            }
            Ok(())
        })?;

        let (file_wal, _replay) = FileWal::open_for_recovery(dir, wal_config)?;
        let committer = GroupCommitter::new(file_wal)?;
        let pool = Arc::new(BatchCoordinatorPool::new(committer, pool_config)?);

        let sstables_dir = dir.join("sstables");
        let (mut manifest_handle, replay_result) = Manifest::open_after_exclusive_lock(dir)?;
        let manifest_edits_replayed = replay_result.edit_count;
        let mut manifest_state = replay_result.state;
        let checkpoint_seq_value = manifest_state.checkpoint_seq();
        let (reconciled_tables, next_id) = reconcile_sstables_with_manifest(
            &sstables_dir,
            &mut manifest_handle,
            &mut manifest_state,
        )?;

        let immutables = Arc::new(RwLock::new(VecDeque::new()));
        let sstables = Arc::new(RwLock::new(reconciled_tables));
        let next_sstable_id = Arc::new(AtomicU64::new(next_id));
        let manifest = Arc::new(Mutex::new(manifest_handle));
        let checkpoint_seq = Arc::new(AtomicU64::new(checkpoint_seq_value));

        let (flush_sender, flush_receiver) = mpsc::channel::<FlushMsg>();
        let flush_stop = Arc::new(AtomicBool::new(false));
        let flush_delay_ms = Arc::new(AtomicU64::new(0));
        let flush_fault_hook: Arc<Mutex<Option<FlushFaultHook>>> = Arc::new(Mutex::new(None));
        let flush_io_fault_hook: Arc<Mutex<Option<FlushIoFaultHook>>> = Arc::new(Mutex::new(None));
        let storage_state = Arc::new(AtomicU8::new(StorageState::Healthy as u8));
        let storage_pressure_events = Arc::new(AtomicU64::new(0));
        let flush_completions = Arc::new(AtomicU64::new(0));
        let snapshot_registry = Arc::new(SnapshotRegistry::default());
        let compaction_fault_hook: Arc<Mutex<Option<CompactionFaultHook>>> =
            Arc::new(Mutex::new(None));
        let compaction_io_fault_hook: Arc<Mutex<Option<CompactionIoFaultHook>>> =
            Arc::new(Mutex::new(None));
        let pending_compaction_deletes: Arc<Mutex<Vec<Arc<SsTable>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let compaction_running = Arc::new(AtomicBool::new(false));
        let compaction_stop = Arc::new(AtomicBool::new(false));
        let compaction_metrics = Arc::new(CompactionMetricCounters::default());

        // `ADR-COMPACTION-001` Increment 2 §4/§16: capacity-1 so a
        // `MaybeCompact` already sitting unconsumed coalesces any
        // further notifications (`try_send` from the flush thread
        // below simply drops a send that would exceed capacity 1,
        // rather than blocking or queueing unboundedly).
        let (compaction_sender, compaction_receiver) = mpsc::sync_channel::<CompactionMsg>(1);

        let flush_handle = spawn_flush_thread(
            flush_receiver,
            Arc::clone(&sstables),
            Arc::clone(&immutables),
            sstables_dir.clone(),
            Arc::clone(&next_sstable_id),
            SsTableWriterConfig {
                target_block_size: lsm_config.sstable_target_block_size,
                bloom_bits_per_key: lsm_config.bloom_bits_per_key,
            },
            lsm_config.max_flush_retries,
            Arc::clone(&flush_stop),
            Arc::clone(&flush_delay_ms),
            Arc::clone(&pool),
            Arc::clone(&manifest),
            Arc::clone(&checkpoint_seq),
            Arc::clone(&flush_fault_hook),
            Arc::clone(&flush_io_fault_hook),
            Arc::clone(&storage_state),
            Arc::clone(&storage_pressure_events),
            lsm_config.storage_pressure_retry_interval,
            compaction_sender.clone(),
            Arc::clone(&flush_completions),
        );

        // `ADR-COMPACTION-001` Increment 2 §9/§14: only spawned when
        // `compaction_auto_trigger` is set (default `false` -- see that
        // field's own doc comment) -- `None` otherwise, mirroring
        // `flush_handle`'s own existing "no thread,
        // shutdown's join step is a no-op" precedent exactly. `compact_
        // once` (the manual entry point) is unaffected either way.
        let compaction_handle = if lsm_config.compaction_auto_trigger {
            Some(spawn_compaction_thread(
                compaction_receiver,
                Arc::clone(&sstables),
                Arc::clone(&next_sstable_id),
                sstables_dir.clone(),
                Arc::clone(&manifest),
                lsm_config.compaction_trigger_count,
                lsm_config.sstable_target_block_size,
                lsm_config.bloom_bits_per_key,
                Arc::clone(&snapshot_registry),
                Arc::clone(&storage_state),
                Arc::clone(&compaction_fault_hook),
                Arc::clone(&compaction_io_fault_hook),
                Arc::clone(&pending_compaction_deletes),
                Arc::clone(&compaction_running),
                Arc::clone(&compaction_stop),
                lsm_config.storage_pressure_retry_interval,
                Arc::clone(&compaction_metrics),
            ))
        } else {
            None
        };

        let recovery_stats = RecoveryStats {
            wal_records_visited,
            wal_records_applied,
            wal_records_skipped_by_checkpoint,
            checkpoint_markers_replayed,
            manifest_edits_replayed,
            recovery_duration: recovery_started.elapsed(),
        };

        Ok(LsmEngine {
            pool,
            active: RwLock::new(active),
            immutables,
            sstables,
            next_sstable_id,
            sstables_dir,
            manifest,
            checkpoint_seq,
            recovery_stats,
            flush_sender,
            flush_handle: Mutex::new(Some(flush_handle)),
            flush_stop,
            flush_delay_ms,
            capacity_pressure_events: AtomicU64::new(0),
            flush_fault_hook,
            storage_state,
            storage_pressure_events,
            flush_completions,
            flush_io_fault_hook,
            snapshot_registry,
            read_stats: Arc::new(ReadStatCounters::default()),
            config: lsm_config,
            compaction_fault_hook,
            compaction_io_fault_hook,
            pending_compaction_deletes,
            compaction_running,
            compaction_sender,
            compaction_handle: Mutex::new(compaction_handle),
            compaction_stop,
            compaction_metrics,
        })
    }

    /// Installs a flush-thread fault hook, called at every
    /// `FlushFaultPoint` the background flush thread's current attempt
    /// reaches, until `clear_flush_fault_hook` is called. Mirrors
    /// `GroupCommitter::install_fsync_fault_hook`'s existing, established
    /// shape and rationale (scoped to this instance, not a process-wide
    /// global — `cargo test` runs tests concurrently by default).
    /// Ordinary production code never calls this.
    pub fn install_flush_fault_hook(&self, hook: impl Fn(FlushFaultPoint) + Send + Sync + 'static) {
        *self
            .flush_fault_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Box::new(hook));
    }

    /// Removes any hook installed by `install_flush_fault_hook`,
    /// restoring the default no-op behavior.
    pub fn clear_flush_fault_hook(&self) {
        *self
            .flush_fault_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// `ADR-WE-SP-001` §16: installs a hook that, while set, replaces the
    /// SSTable-write step of every flush attempt with a synthetic
    /// `io::Error` whenever it returns `Some`. Mirrors `install_flush_
    /// fault_hook`'s shape; ordinary production code never calls this.
    pub fn install_flush_io_fault_hook(
        &self,
        hook: impl Fn() -> Option<io::Error> + Send + Sync + 'static,
    ) {
        *self
            .flush_io_fault_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Box::new(hook));
    }

    /// Removes any hook installed by `install_flush_io_fault_hook`.
    pub fn clear_flush_io_fault_hook(&self) {
        *self
            .flush_io_fault_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// `ADR-COMPACTION-001` Decision 15. Mirrors `install_flush_fault_
    /// hook`'s exact shape and rationale. Ordinary production code
    /// never calls this.
    pub fn install_compaction_fault_hook(
        &self,
        hook: impl Fn(CompactionFaultPoint) + Send + Sync + 'static,
    ) {
        *self
            .compaction_fault_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Box::new(hook));
    }

    /// Removes any hook installed by `install_compaction_fault_hook`.
    pub fn clear_compaction_fault_hook(&self) {
        *self
            .compaction_fault_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// `ADR-COMPACTION-001` Decision 15. Mirrors `install_flush_io_
    /// fault_hook`'s exact shape; injected immediately before `compact_
    /// once` calls `sstable::write_from_sorted_records`. Ordinary
    /// production code never calls this.
    pub fn install_compaction_io_fault_hook(
        &self,
        hook: impl Fn() -> Option<io::Error> + Send + Sync + 'static,
    ) {
        *self
            .compaction_io_fault_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Box::new(hook));
    }

    /// Removes any hook installed by `install_compaction_io_fault_hook`.
    pub fn clear_compaction_io_fault_hook(&self) {
        *self
            .compaction_io_fault_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// `ADR-WE-SP-001` §14: current storage-health state.
    pub fn storage_state(&self) -> StorageState {
        StorageState::from_u8(self.storage_state.load(Ordering::Acquire))
    }

    /// Count of ENOSPC-classified flush failures observed so far
    /// (`ADR-WE-SP-001` §14) — distinct from `capacity_pressure_events`.
    pub fn storage_pressure_events(&self) -> u64 {
        self.storage_pressure_events.load(Ordering::Relaxed)
    }

    /// Count of flush jobs that have *fully* settled (see the increment
    /// site in `spawn_flush_thread` for exactly what "settled" bounds).
    /// Test/observability-only.
    #[allow(dead_code)]
    pub(crate) fn flush_completions(&self) -> u64 {
        self.flush_completions.load(Ordering::Acquire)
    }

    /// `ADR-WE-SP-001` §9: once `StorageState::StorageFull` is confirmed,
    /// a new write fails fast with `StorageExhausted` *before* any WAL
    /// append is attempted — the engine already has strong evidence
    /// (an immutable backlog stuck at its configured bound behind a
    /// flush that is itself stuck on a confirmed storage-capacity
    /// error) that the append would only repeat the same failure.
    fn reject_if_storage_full(&self) -> Result<()> {
        if self.storage_state() == StorageState::StorageFull {
            return Err(EngineError::StorageExhausted {
                detail: "persistent storage is exhausted (STORAGE_FULL); write rejected before \
                         WAL append -- retry once storage_state() shows recovery toward Healthy"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Test-only: makes every subsequent flush attempt sleep `delay`
    /// before running, so a test can reliably widen the window in which
    /// `immutables`/memory-accounting state can be observed before the
    /// background flush thread drains it — without this, such a test
    /// would depend on winning an incidental race against real disk I/O
    /// (`PHASE4B_ARCHITECTURE.md` §8/§44).
    #[cfg(any(test, feature = "test-util"))]
    pub fn set_flush_delay_for_test(&self, delay: Duration) {
        self.flush_delay_ms
            .store(delay.as_millis() as u64, Ordering::Release);
    }

    /// Test-only: forces `storage_state()` directly, so a test can
    /// exercise `compact_once`'s "observe `StoragePressure`/
    /// `StorageFull`, defer/skip" behavior (`ADR-COMPACTION-001`
    /// Decision 11) deterministically, without needing to actually
    /// exhaust disk space. Never called by production code.
    #[cfg(any(test, feature = "test-util"))]
    pub fn set_storage_state_for_test(&self, state: StorageState) {
        self.storage_state.store(state as u8, Ordering::Release);
    }

    /// `Put(key, value)` — operating brief §12. Submits through the
    /// unmodified Dedicated Batch Coordinator, waits for WAL durability
    /// on the calling thread, then applies to the MemTable. Returns the
    /// assigned sequence number once fully durable *and* applied (i.e.
    /// once this call returns, both are guaranteed — `PHASE4A_
    /// ARCHITECTURE.md` §5's "logical completion" point).
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<u64> {
        self.reject_if_storage_full()?;
        let completion = self.pool.submit(WalOpOwned::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        })?;
        let position = completion.wait()?;
        self.apply_after_durable(key, position.seq, MemtableValue::Put(value.to_vec()))?;
        Ok(position.seq)
    }

    /// `Delete(key)` — operating brief §13. Represented as a
    /// `MemtableValue::Tombstone`; older entries for this key are never
    /// physically removed (required for future SSTable/compaction
    /// semantics, LSM Engine Spec §1.4) — this call only ever adds a new,
    /// newer entry.
    pub fn delete(&self, key: &[u8]) -> Result<u64> {
        self.reject_if_storage_full()?;
        let completion = self.pool.submit(WalOpOwned::Delete { key: key.to_vec() })?;
        let position = completion.wait()?;
        self.apply_after_durable(key, position.seq, MemtableValue::Tombstone)?;
        Ok(position.seq)
    }

    /// Applies one already-durable entry to the active MemTable, then
    /// freezes it if it just became full — one critical section under
    /// the write lock, so a concurrent freeze race between two callers
    /// observing "full" at the same time cannot happen (operating brief
    /// §19).
    fn apply_after_durable(&self, key: &[u8], seq: u64, value: MemtableValue) -> Result<()> {
        let mut active = self.lock_active_write();
        active.insert(key, seq, value);
        if active.is_full() {
            self.freeze_locked(&mut active)?;
        }
        Ok(())
    }

    /// Freezes the current active MemTable and installs a fresh one in
    /// its place, under the caller's already-held write guard — atomic
    /// with respect to concurrent writers (operating brief §19's own
    /// requirement), since no other thread can be inside `apply_after_
    /// durable` while this guard is held. Checks immutable backpressure
    /// (operating brief §21) *before* freezing — a caller that hits the
    /// limit gets a clean, documented error and its own already-applied
    /// insert is **not** rolled back (the entry is durable and now live
    /// in what remains the active MemTable; only the *freeze* is
    /// refused, not the write that triggered it). This is the accepted,
    /// twice-ratified contract (`PHASE4A_FAILURE_MODEL.md` §2,
    /// `PHASE4A_ADR.md` ADR-P4A-5, reaffirmed as "transient rather than
    /// permanent" by `PHASE4B_ADR.md`) — `CapacityExceeded` is still
    /// returned to the caller; only its *meaning* (backpressure signal,
    /// not data loss) and its *duration* (cleared once the background
    /// flush thread catches up) changed across phases. Do not change
    /// this to a silent `Ok` — that would contradict both ratified ADRs.
    /// `capacity_pressure_events` below is purely additive observability
    /// alongside the existing `Err`, not a replacement for it.
    fn freeze_locked(&self, active_guard: &mut MemTable) -> Result<()> {
        let mut immutables = self.lock_immutables_write();
        if immutables.len() >= self.config.max_immutable_memtables {
            self.capacity_pressure_events
                .fetch_add(1, Ordering::Relaxed);
            // `ADR-WE-SP-001` §6.3: the immutable backlog reaching its
            // configured bound while the flush thread is already stuck
            // in `StoragePressure` is exactly the "safe resource
            // boundary" the ADR names as the `StorageFull` trigger --
            // from here, `reject_if_storage_full` fails new writes fast
            // instead of letting applied-but-unflushed state keep
            // growing. Never demotes: only the flush thread's own
            // success path ever clears `StorageFull` (see
            // `spawn_flush_thread`). The existing `CapacityExceeded`
            // return below is unchanged either way -- this promotion is
            // purely additive, per the ADR's explicit instruction not to
            // repurpose that established contract.
            if self
                .storage_state
                .compare_exchange(
                    StorageState::StoragePressure as u8,
                    StorageState::StorageFull as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                eprintln!(
                    "rubixdb: storage state STORAGE_PRESSURE -> STORAGE_FULL (immutable backlog \
                     reached max_immutable_memtables={}); new writes will fail fast with \
                     StorageExhausted until a flush succeeds",
                    self.config.max_immutable_memtables
                );
            }
            return Err(EngineError::CapacityExceeded {
                requested: (immutables.len() as u64) + 1,
                max: self.config.max_immutable_memtables as u64,
            });
        }
        let fresh = MemTable::new(self.config.memtable_max_size_bytes);
        let old = std::mem::replace(active_guard, fresh).freeze();
        immutables.push_front(Arc::clone(&old));
        // Never fails: an unbounded `mpsc::Sender` only errors if the
        // receiver has been dropped, which happens only after `shutdown`
        // has already joined the flush thread — by which point no further
        // `put`/`delete` (and therefore no further freeze) can legitimately
        // still be in flight through this same `&self`. A send failure
        // here would mean that invariant broke, not a normal runtime
        // condition to route through `Result` -- surfaced via `expect`
        // rather than silently dropped (never silently discard an
        // ImmutableMemTable, operating brief §25).
        self.flush_sender
            .send(FlushMsg::Flush(old))
            .expect("flush thread must outlive every in-flight write");
        Ok(())
    }

    /// `Get(key)` — latest visible value as of "now." Equivalent to
    /// `get_as_of(key, u64::MAX)`.
    pub fn get(&self, key: &[u8]) -> Result<GetResult> {
        self.get_as_of(key, u64::MAX)
    }

    /// `Get(key, snapshot_sequence)` — operating brief §14/§16/§21.
    /// Consults the active MemTable, then each immutable newest-to-oldest,
    /// then (Phase 4B) each SSTable newest-to-oldest — the first source
    /// with *any* version at `seq <= as_of_seq` is authoritative; older
    /// sources are never consulted once a hit is found (LSM Engine Spec
    /// §4.2's `ReadView` merge rule). Fallible as of Phase 4B: an SSTable
    /// read can fail closed on corruption
    /// (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §6) — a possibility the
    /// MemTable-only Phase 4A read path never had, so this can no longer
    /// be an infallible lookup.
    pub fn get_as_of(&self, key: &[u8], as_of_seq: u64) -> Result<GetResult> {
        // `ADR-RE-001` §12/Implementation Increment 1: `ReadStats`
        // instrumentation only -- every increment below sits alongside an
        // existing branch/return, never changes which branch is taken or
        // what is returned. `read_hits` counts "some source held a
        // matching (key, seq) entry" (a found tombstone counts as a hit
        // here, even though it still resolves to `Ok(None)` below, same
        // as it always has) -- see `ReadStats`'s own doc comment.
        self.read_stats.requests.fetch_add(1, Ordering::Relaxed);
        {
            let active = self.lock_active_read();
            if let Some((_, value)) = active.get_as_of(key, as_of_seq) {
                self.read_stats.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(resolve(value));
            }
        }
        {
            let immutables = self.lock_immutables_read();
            for imm in immutables.iter() {
                if let Some((_, value)) = imm.get_as_of(key, as_of_seq) {
                    self.read_stats.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(resolve(value));
                }
            }
        }
        {
            let sstables = self.lock_sstables_read();
            for table in sstables.iter() {
                self.read_stats
                    .sstables_consulted
                    .fetch_add(1, Ordering::Relaxed);
                if let Some((_, value)) = table.get_versioned(key, as_of_seq)? {
                    self.read_stats.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(resolve_sstable(value));
                }
            }
        }
        self.read_stats.misses.fetch_add(1, Ordering::Relaxed);
        Ok(None)
    }

    /// `ADR-RE-001` §2/§10: `Contains(key, snapshot_sequence)` — visible
    /// existence only, `true` iff some source's first (recency-ordered)
    /// match for `key` at `as_of_seq` is a live `Put`, `false` for a
    /// missing key *or* a visible tombstone. Deliberately mirrors
    /// `get_as_of`'s exact source order and first-hit-wins rule line for
    /// line (active → immutables newest-first → SSTables newest-first)
    /// rather than sharing a helper with it, so the equivalence
    /// `contains(k, s) == get_as_of(k, s)?.is_some()` holds by
    /// construction and is exercised directly as a test, not merely
    /// implemented as a wrapper — the SSTable step calls
    /// `contains_versioned`, never `get_versioned`, so no value bytes
    /// are copied on this path (see that method's own doc comment for
    /// what that saving actually amounts to, measured in
    /// `PHASE_READ_ENGINE_PERFORMANCE.md`).
    pub fn contains(&self, key: &[u8], as_of_seq: u64) -> Result<bool> {
        self.read_stats.requests.fetch_add(1, Ordering::Relaxed);
        {
            let active = self.lock_active_read();
            if let Some((_, value)) = active.get_as_of(key, as_of_seq) {
                self.read_stats.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(matches!(value, MemtableValue::Put(_)));
            }
        }
        {
            let immutables = self.lock_immutables_read();
            for imm in immutables.iter() {
                if let Some((_, value)) = imm.get_as_of(key, as_of_seq) {
                    self.read_stats.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(matches!(value, MemtableValue::Put(_)));
                }
            }
        }
        {
            let sstables = self.lock_sstables_read();
            for table in sstables.iter() {
                self.read_stats
                    .sstables_consulted
                    .fetch_add(1, Ordering::Relaxed);
                if let Some(is_put) = table.contains_versioned(key, as_of_seq)? {
                    self.read_stats.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(is_put);
                }
            }
        }
        self.read_stats.misses.fetch_add(1, Ordering::Relaxed);
        Ok(false)
    }

    /// A snapshot boundary consistent with `PHASE4A_ARCHITECTURE.md` §5's
    /// ordering rule: only durable data is ever snapshot-visible. Callers
    /// wanting "as of right now, including whatever I just wrote" should
    /// use the `u64` this method returns immediately after their own
    /// `put`/`delete` call, not a value captured beforehand.
    pub fn snapshot_seq(&self) -> u64 {
        self.pool.stats().committer_stats.durable_through
    }

    /// `ADR-RE-001` §2: takes a registered, `Drop`-released read
    /// snapshot pinned at the current durable watermark (the same value
    /// `snapshot_seq()` returns) -- pass `Snapshot::seq()` to
    /// `get_as_of` (and, once implemented, `range_scan`) for a stable,
    /// point-in-time read. Unlike a bare `snapshot_seq()` value, holding
    /// a `Snapshot` is visible to `oldest_live_snapshot_seq()` -- the
    /// piece of information a future Compaction phase will need and
    /// that this phase deliberately does not yet act on (no Compaction
    /// exists to consult it).
    pub fn snapshot(&self) -> Snapshot {
        let seq = self.snapshot_seq();
        self.snapshot_registry.acquire(seq);
        Snapshot {
            seq,
            registry: Arc::clone(&self.snapshot_registry),
        }
    }

    /// `ADR-RE-001` §2: the lowest sequence any currently-live
    /// [`Snapshot`] still holds, or `None` if none are outstanding.
    pub fn oldest_live_snapshot_seq(&self) -> Option<u64> {
        self.snapshot_registry.oldest()
    }

    /// `ADR-RE-001` §12: a point-in-time copy of the read-path counters.
    /// `bloom_negatives`/`blocks_read` are summed across every
    /// currently-live `SsTable` at call time (each is individually
    /// cumulative per table; since no table is ever removed without
    /// Compaction, summing the live set is exactly the lifetime total --
    /// see `ReadStats`'s own doc comment).
    pub fn read_stats(&self) -> ReadStats {
        let (bloom_negatives, blocks_read) = {
            let sstables = self.lock_sstables_read();
            sstables.iter().fold((0u64, 0u64), |(bn, br), table| {
                (bn + table.bloom_negative_count(), br + table.blocks_read())
            })
        };
        ReadStats {
            read_requests: self.read_stats.requests.load(Ordering::Relaxed),
            read_hits: self.read_stats.hits.load(Ordering::Relaxed),
            read_misses: self.read_stats.misses.load(Ordering::Relaxed),
            bloom_negatives,
            blocks_read,
            sstables_consulted: self.read_stats.sstables_consulted.load(Ordering::Relaxed),
        }
    }

    /// `ADR-RE-001` §1/§5: captures a [`ReadView`] over `[start, end)` --
    /// the consistency foundation `range_scan` builds on (`pub(crate)`,
    /// not itself part of the public read API). Three short
    /// lock-acquire/clone-or-extract/release scopes, never more than one
    /// lock held at a time, matching this codebase's existing
    /// lock-scoping discipline (`lock_active_read`/`lock_immutables_
    /// read`/`lock_sstables_read`).
    pub(crate) fn capture_read_view(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> ReadView {
        if range_is_definitely_empty(start, end) {
            return ReadView {
                active_range: Vec::new(),
                immutables: Vec::new(),
                sstables: Vec::new(),
            };
        }
        let active_range: Vec<((Vec<u8>, u64), MemtableValue)> = {
            let active = self.lock_active_read();
            active
                .range(start, end)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        let immutables: Vec<Arc<MemTable>> = {
            let guard = self.lock_immutables_read();
            guard.iter().cloned().collect()
        };
        let sstables: Vec<Arc<SsTable>> = {
            let guard = self.lock_sstables_read();
            guard.iter().cloned().collect()
        };
        ReadView {
            active_range,
            immutables,
            sstables,
        }
    }

    /// `ADR-RE-001` §3/§13: the k-way-merged, version-resolved,
    /// tombstone-collapsed range read -- the `LsmEngine`-level
    /// counterpart to `get_as_of`, covering a key range instead of one
    /// key. Captures a [`ReadView`] once, here, before returning (every
    /// engine lock is released before the first item is ever produced);
    /// iteration afterward touches only that captured view. `read_
    /// requests` counts this call once, regardless of how many rows the
    /// returned iterator ultimately yields (`ADR-RE-001` §13's explicit
    /// instruction: one invocation = one request, never one per row).
    pub fn range_scan(
        &self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        as_of_seq: u64,
    ) -> RangeScanIter {
        self.read_stats.requests.fetch_add(1, Ordering::Relaxed);
        let owned_start = to_owned_bound(start);
        let owned_end = to_owned_bound(end);
        let read_view = self.capture_read_view(start, end);
        RangeScanIter::new(
            read_view,
            owned_start,
            owned_end,
            as_of_seq,
            Arc::clone(&self.read_stats),
        )
    }

    /// `range_scan` at `as_of_seq = u64::MAX` -- "as of right now,"
    /// mirroring `get`'s own relationship to `get_as_of` exactly
    /// (`get(key) = get_as_of(key, u64::MAX)`, `src/lsm/mod.rs`).
    pub fn range(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> RangeScanIter {
        self.range_scan(start, end, u64::MAX)
    }

    /// Current active MemTable's byte size — observability only
    /// (operating brief §17, exposed at the engine level for tests/
    /// diagnostics without requiring a caller to reach into the lock
    /// itself).
    pub fn active_size_bytes(&self) -> usize {
        self.lock_active_read().size_bytes()
    }

    pub fn active_entry_count(&self) -> usize {
        self.lock_active_read().entry_count()
    }

    pub fn immutable_count(&self) -> usize {
        self.lock_immutables_read().len()
    }

    /// Number of `put`/`delete` calls so far that returned
    /// `Err(CapacityExceeded)` because their triggering freeze found
    /// `immutables` at `max_immutable_memtables` (`freeze_locked`).
    /// Purely additive observability: the underlying write was still
    /// durable in the WAL and applied to the active MemTable before this
    /// counted (`PHASE4A_FAILURE_MODEL.md` §2). A sustained non-zero
    /// rate indicates the background flush thread is not keeping up with
    /// the write rate.
    pub fn capacity_pressure_events(&self) -> u64 {
        self.capacity_pressure_events.load(Ordering::Relaxed)
    }

    /// Total bytes retained across every immutable MemTable — memory
    /// accounting must remain correct after freeze (operating brief
    /// §19), verified directly by this accessor in tests.
    pub fn immutable_total_bytes(&self) -> usize {
        self.lock_immutables_read()
            .iter()
            .map(|m| m.size_bytes())
            .sum()
    }

    /// Number of published SSTables currently in the read path —
    /// observability only (operating brief §17/§40).
    pub fn sstable_count(&self) -> usize {
        self.lock_sstables_read().len()
    }

    /// `<data_dir>/sstables` — observability/testing only.
    pub fn sstables_dir(&self) -> &Path {
        &self.sstables_dir
    }

    /// The next id a flush will assign — observability/testing only
    /// (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.1).
    pub fn next_sstable_id(&self) -> u64 {
        self.next_sstable_id.load(Ordering::SeqCst)
    }

    /// The current durable Manifest checkpoint (`flushed_through_seq`),
    /// `0` if none has ever been recorded — `RUBIC_MANIFEST_FORMAT_
    /// SPECIFICATION.md` §5's monotonic durable boundary. Observability
    /// only; the write path never derives correctness decisions from
    /// this accessor (it reads the flush thread's own already-durable
    /// value, never the other way around).
    pub fn checkpoint_seq(&self) -> u64 {
        self.checkpoint_seq.load(Ordering::Acquire)
    }

    /// Observability for the `LsmEngine::open` call that produced this
    /// instance — `RecoveryStats`'s own doc comment has the field-by-
    /// field detail.
    pub fn recovery_stats(&self) -> RecoveryStats {
        self.recovery_stats
    }

    /// Manifest inspection surface (operating brief: "engineers to
    /// inspect... without mutating storage") — every accessor here is
    /// read-only, reads the already-open, already-locked `Manifest`
    /// handle, and is safe to call from a running engine or a
    /// diagnostic tool built on top of it.
    pub fn manifest_record_count(&self) -> u64 {
        self.manifest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .record_count()
    }

    pub fn manifest_size_bytes(&self) -> Result<u64> {
        self.manifest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .size_bytes()
    }

    pub fn manifest_last_edit(&self) -> Option<ManifestEdit> {
        self.manifest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last_edit()
    }

    /// Live SSTable ids currently in the read path, newest-first —
    /// inspection only.
    pub fn live_sstable_ids(&self) -> Vec<u64> {
        self.lock_sstables_read().iter().map(|t| t.id()).collect()
    }

    /// `ADR-COMPACTION-001` Decision 14: the deterministic trigger
    /// *check* only — pure, no side effect. `LsmConfig.compaction_
    /// trigger_count` (default 4, LSM Engine Spec §5.1). As of
    /// Increment 2, this is also what the background compaction
    /// worker's own internal check mirrors (`compact_once_impl`) --
    /// this method itself remains a manual, directly-callable entry
    /// point regardless of whether automatic triggering is enabled,
    /// deliberately preserved for tests and any possible future direct/
    /// on-demand use, not itself called by the automatic path (which
    /// uses its own independent field clones via the free function,
    /// never through `&self`) -- hence still no non-test caller.
    #[allow(dead_code)]
    pub(crate) fn should_compact(&self) -> bool {
        self.sstable_count() >= self.config.compaction_trigger_count
    }

    /// `ADR-COMPACTION-001`: one full, synchronous, deterministic
    /// compaction cycle — the "core operation" Decision 13/Decision 14
    /// define, unchanged in behavior since Increment 1. As of Increment
    /// 2, this is a thin wrapper over `compact_once_impl` (the same
    /// free function the background compaction worker thread also
    /// calls, with its own cloned field `Arc`s) — safe to call manually
    /// at any time, from any thread, regardless of whether automatic
    /// triggering (`compaction_auto_trigger`) is enabled: `compact_
    /// once_impl`'s own `CompactionRunGuard` (Increment 2 §4) makes any
    /// two concurrent callers (this method and/or the worker)
    /// mutually exclusive, never racing, never double-compacting.
    #[allow(dead_code)] // see `should_compact`'s own doc comment
    pub(crate) fn compact_once(&self) -> Result<Option<(SstableMeta, CompactionStats)>> {
        compact_once_impl(
            &self.sstables,
            &self.next_sstable_id,
            &self.sstables_dir,
            &self.manifest,
            self.config.compaction_trigger_count,
            self.config.sstable_target_block_size,
            self.config.bloom_bits_per_key,
            &self.snapshot_registry,
            &self.storage_state,
            &self.compaction_fault_hook,
            &self.compaction_io_fault_hook,
            &self.pending_compaction_deletes,
            &self.compaction_running,
            &self.compaction_metrics,
        )
    }

    /// Increment 3: a point-in-time snapshot of cumulative Compaction
    /// health metrics — see `CompactionMetrics`'s own doc comment.
    /// Cheap (atomic loads plus one small `Mutex` lock for `last_
    /// cycle`), safe to call frequently (e.g. from a soak harness's own
    /// periodic sampling loop), and never consulted by any production
    /// decision — purely observational, same status `read_stats()`
    /// already has.
    pub fn compaction_metrics(&self) -> CompactionMetrics {
        let c = &self.compaction_metrics;
        CompactionMetrics {
            cycles_completed: c.cycles_completed.load(Ordering::Relaxed),
            input_sstables_total: c.input_sstables_total.load(Ordering::Relaxed),
            input_bytes_total: c.input_bytes_total.load(Ordering::Relaxed),
            output_bytes_total: c.output_bytes_total.load(Ordering::Relaxed),
            records_read_total: c.records_read_total.load(Ordering::Relaxed),
            records_retained_total: c.records_retained_total.load(Ordering::Relaxed),
            records_dropped_total: c.records_dropped_total.load(Ordering::Relaxed),
            tombstones_dropped_total: c.tombstones_dropped_total.load(Ordering::Relaxed),
            versions_dropped_total: c.versions_dropped_total.load(Ordering::Relaxed),
            duration_total: Duration::from_nanos(c.duration_total_nanos.load(Ordering::Relaxed)),
            duration_max: Duration::from_nanos(c.duration_max_nanos.load(Ordering::Relaxed)),
            peak_temp_disk_bytes_max: c.peak_temp_disk_bytes_max.load(Ordering::Relaxed),
            last_cycle: c
                .last_cycle
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
        }
    }

    /// Read-only access to the underlying pool's own stats — reuses the
    /// existing, unmodified observability surface rather than
    /// duplicating it.
    pub fn pool_stats(&self) -> crate::execution::batch_coordinator::BatchCoordinatorStats {
        self.pool.stats()
    }

    /// Delegates to the unmodified `BatchCoordinatorPool::shutdown` for
    /// the write path, then stops the background flush thread cleanly:
    /// sets `flush_stop` (so a persistently-failing flush's retry loop
    /// exits promptly rather than blocking shutdown indefinitely,
    /// `PHASE4B_ARCHITECTURE.md` §8), sends an explicit `Shutdown`
    /// message (in case the thread is idle, blocked in `recv`), and
    /// joins it. A second call, or an engine built without a flush
    /// thread (`lsm::tests`'s raw-construction fault-injection test),
    /// finds `flush_handle` already `None` and is a no-op for this part.
    /// `ADR-COMPACTION-001` Increment 2 §9's exact shutdown contract,
    /// determined before coding, not improvised: new compaction work
    /// stops being scheduled (`compaction_stop` set first, before the
    /// wake-up signal, so the worker's own next loop iteration -- even
    /// one already past its wake and mid-decision -- observes it before
    /// starting another cycle); an already-running compaction is
    /// **not** aborted mid-cycle (mirrors `flush_handle`'s own
    /// established precedent: `compact_once_impl` has no internal
    /// cancellation point, and letting it complete is strictly safer
    /// than interrupting a partially-published output -- Increment 1's
    /// own crash-window tests already prove every interruption point is
    /// recoverable, but "recoverable via a real crash" is not the same
    /// contract as "safe to interrupt via a normal, orderly shutdown
    /// when interruption is entirely avoidable"); no worker is leaked
    /// (`compaction_handle.take()` + `join()`, unconditional); no join
    /// deadlock (the worker's own loop checks `compaction_stop` right
    /// after finishing its current cycle and before starting another,
    /// so `join()` is bounded by "however long the *current* cycle
    /// takes," never indefinite); no partially-published output or lost
    /// Manifest state (unaffected by shutdown timing at all -- `compact_
    /// once_impl`'s own atomic-construction/Manifest-fsync discipline,
    /// Increment 1, already guarantees this regardless of when the
    /// process stops).
    pub fn shutdown(&self) -> crate::execution::batch_coordinator::ShutdownReportBC {
        let report = self.pool.shutdown();
        self.flush_stop.store(true, Ordering::Release);
        let _ = self.flush_sender.send(FlushMsg::Shutdown);
        let handle = self
            .flush_handle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }

        self.compaction_stop.store(true, Ordering::Release);
        // A blocking `send`, not `try_send`: the bounded(1) channel may
        // already hold an unconsumed `MaybeCompact` (a `try_send` here
        // would then silently drop `Shutdown`, leaving the worker to
        // notice the stop request only via its own periodic fallback
        // tick, up to `storage_pressure_retry_interval` later -- a real
        // slowdown found empirically, not merely theoretical, once
        // automatic triggering was exercised under real write load).
        // `send` blocks only until the worker's own loop drains the
        // channel (which it always promptly does, whether or not the
        // wake found real work), or returns immediately with an
        // ignored `Err` if the worker has already exited on its own
        // (a dropped `Receiver` disconnects the channel, `send` never
        // blocks against a disconnected receiver).
        let _ = self.compaction_sender.send(CompactionMsg::Shutdown);
        let compaction_handle = self
            .compaction_handle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(handle) = compaction_handle {
            let _ = handle.join();
        }

        report
    }

    fn lock_active_write(&self) -> std::sync::RwLockWriteGuard<'_, MemTable> {
        self.active.write().unwrap_or_else(|p| p.into_inner())
    }
    fn lock_active_read(&self) -> std::sync::RwLockReadGuard<'_, MemTable> {
        self.active.read().unwrap_or_else(|p| p.into_inner())
    }
    fn lock_immutables_write(&self) -> std::sync::RwLockWriteGuard<'_, VecDeque<Arc<MemTable>>> {
        self.immutables.write().unwrap_or_else(|p| p.into_inner())
    }
    fn lock_immutables_read(&self) -> std::sync::RwLockReadGuard<'_, VecDeque<Arc<MemTable>>> {
        self.immutables.read().unwrap_or_else(|p| p.into_inner())
    }
    fn lock_sstables_read(&self) -> std::sync::RwLockReadGuard<'_, Vec<Arc<SsTable>>> {
        self.sstables.read().unwrap_or_else(|p| p.into_inner())
    }
}

fn resolve(value: &MemtableValue) -> GetResult {
    match value {
        MemtableValue::Put(v) => Some(v.clone()),
        MemtableValue::Tombstone => None,
    }
}

fn resolve_sstable(value: RecordValue) -> GetResult {
    match value {
        RecordValue::Put(v) => Some(v),
        RecordValue::Tombstone => None,
    }
}

fn wrap_sstable_path_error(e: EngineError, path: &Path) -> EngineError {
    match e {
        EngineError::Corruption { detail } => EngineError::Corruption {
            detail: format!("sstable {}: {detail}", path.display()),
        },
        EngineError::Unsupported { operation } => EngineError::Unsupported {
            operation: format!("sstable {}: {operation}", path.display()),
        },
        other => other,
    }
}

/// The Manifest-authoritative directory reconciliation sweep
/// (`PHASE5_MANIFEST_ARCHITECTURE.md` §4 step 6a, `RUBIC_SSTABLE_
/// FORMAT_SPECIFICATION.md` §3.3 / LSM Engine Spec §7.2). Only runs
/// while the caller already holds the exclusive directory lock (via
/// `Manifest::open_after_exclusive_lock`'s own contract, transitively).
///
/// For every `*.sst` file found:
/// - in `state.live_sstables`: open + validate (fail closed on
///   corruption, per `PHASE4B_ADR.md` ADR-P4B-2's precedent — a corrupt
///   *live* SSTable is escalated, never silently excluded);
/// - in `state.ever_added` but not live (a removed table a crash left
///   physically undeleted — unreachable this phase, since nothing
///   issues `REMOVE_SSTABLE` without Compaction, but handled per spec
///   for forward compatibility): deleted;
/// - in neither (durable, valid, but never acknowledged — the crash-
///   between-fsync-and-manifest-write case): validated, a fresh
///   `ADD_SSTABLE` is durably appended for it now, and it joins the
///   live set.
///
/// After the scan: any id `state.live_sstables` claims live but that
/// had no corresponding file on disk fails `open()` closed — "never
/// silently omit a missing live table."
fn reconcile_sstables_with_manifest(
    sstables_dir: &Path,
    manifest: &mut Manifest,
    state: &mut ManifestState,
) -> Result<(Vec<Arc<SsTable>>, u64)> {
    std::fs::create_dir_all(sstables_dir)?;

    let mut seen_ids: HashSet<u64> = HashSet::new();
    let mut max_id_seen: u64 = 0;
    let mut opened: BTreeMap<u64, Arc<SsTable>> = BTreeMap::new();

    for entry in std::fs::read_dir(sstables_dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.ends_with(".sst.tmp") {
            std::fs::remove_file(&path)?;
            continue;
        }
        let Some(id) = sstable::parse_sstable_id(name) else {
            continue;
        };
        max_id_seen = max_id_seen.max(id);
        seen_ids.insert(id);

        if state.live_sstables.contains_key(&id) {
            let table = SsTable::open(&path, id).map_err(|e| wrap_sstable_path_error(e, &path))?;
            opened.insert(id, Arc::new(table));
        } else if state.ever_added.contains(&id) {
            // Removed-but-undeleted orphan (LSM Engine Spec §7.2) —
            // unreachable this phase (no Compaction issues
            // REMOVE_SSTABLE yet), handled for forward compatibility.
            std::fs::remove_file(&path)?;
        } else {
            let table = SsTable::open(&path, id).map_err(|e| wrap_sstable_path_error(e, &path))?;
            let file_size = std::fs::metadata(&path)?.len();
            let edit = ManifestEdit::AddSstable {
                id,
                min_seq: table.min_seq(),
                max_seq: table.max_seq(),
                file_size,
            };
            manifest.append_sync(edit)?;
            state.apply(edit)?;
            opened.insert(id, Arc::new(table));
        }
    }

    for id in state.live_sstables.keys() {
        if !seen_ids.contains(id) {
            return Err(EngineError::Corruption {
                detail: format!(
                    "manifest: sstable id {id} is recorded live but {} is missing from disk",
                    sstables_dir.join(sstable::sstable_filename(*id)).display()
                ),
            });
        }
    }

    let next_id = max_id_seen
        .max(state.ever_added.iter().copied().max().unwrap_or(0))
        .max(state.live_sstables.keys().copied().max().unwrap_or(0))
        + 1;

    let tables: Vec<Arc<SsTable>> = opened.into_iter().rev().map(|(_, t)| t).collect();
    Ok((tables, next_id))
}

/// The background flush thread body (`PHASE4B_ADR.md` ADR-P4B-5,
/// extended by `PHASE5_MANIFEST_ARCHITECTURE.md` §5/§9): drains
/// `immutables` oldest-first, running the full publish -> checkpoint ->
/// purge sequence for each one:
///
/// 1. Build + atomically publish the SSTable (unchanged from Phase 4B,
///    `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.4).
/// 2. Durably append `ADD_SSTABLE` to the Manifest.
/// 3. Make it visible in `sstables` (idempotent: only inserted if not
///    already present, so a retry after step 4+ never double-inserts).
/// 4. `pool.rotate()` (WAL Spec §2.2's own note, ahead of the marker).
/// 5. Durably append `CHECKPOINT_MARKER` to the WAL, via the same
///    `submit`/`wait` path any other write uses — no second sequence
///    system.
/// 6. Durably append `SET_CHECKPOINT` to the Manifest.
/// 7. Update the shared `checkpoint_seq` observability value.
/// 8. Drop the `ImmutableMemTable` — only now.
/// 9. `pool.purge_before(...)` — only now, per the exact WAL Spec §10
///    precondition chain (marker durably written AND SSTable durably
///    Manifest-registered AND checkpoint durably recorded).
///
/// **Idempotent retry** (operating brief: "a failed or retried flush
/// must not create two logically active copies of the same immutable
/// state"): `published`/`checkpoint_marker`/`checkpoint_recorded` each
/// independently track whether *their own* step already durably
/// succeeded for this frozen memtable; a retry (triggered by any later
/// step's failure) only ever repeats steps that have not yet succeeded.
/// This closes a real bug found by this phase's own crash-cycle
/// testing: an earlier version tracked only `published`, so a failure
/// in step 6 (`SET_CHECKPOINT`) after step 5 (`CHECKPOINT_MARKER`) had
/// already durably succeeded caused the retry to durably resubmit a
/// *second* `CHECKPOINT_MARKER` for the same logical flush — safe
/// (never wrong data, never a lost write) but a real, measurable
/// deviation from "idempotent retry," caught by the crash harness's
/// exact-accounting `RecoveryStats`-based invariant, not merely
/// asserted correct (`PHASE5_ADR.md` has the full account). A flush
/// failure can **never** advance `checkpoint_seq` or call
/// `purge_before` — steps 4-9 are structurally unreachable unless steps
/// 1-2 already durably succeeded, and the function returns `Err`
/// (triggering a retry, not partial progress) the instant any step
/// fails.
///
/// **Panic safety** (operating brief: audit the flush-thread-panic gap
/// `PHASE4B_FAILURE_MODEL.md` named — "a flush-thread panic is not
/// automatically recovered... may create an operational failure mode
/// that was tolerable before checkpointing but is not necessarily
/// acceptable now"): each attempt runs inside `catch_unwind`. A panic
/// during one attempt is treated as exactly the same kind of failure as
/// an I/O error — logged, retried with the same backoff policy, using
/// the same per-step idempotence state above (a panic can never leave
/// `published`/`checkpoint_marker`/`checkpoint_recorded` in a state
/// that causes the *next* attempt to redo an already-durable step,
/// since each is only ever set *after* its own step's durable success).
/// This is deliberately **not** a supervised-restart *thread* design
/// (spawning a fresh `JoinHandle` after a fatal error) — the operating
/// brief explicitly warns against an automatic restart that "could
/// duplicate an SSTable or replay an unsafe checkpoint transition," and
/// this design sidesteps that risk entirely by never letting the thread
/// die in the first place: the same one thread, the same one set of
/// idempotence guards, handles both I/O failures and panics uniformly.
/// `ADR-COMPACTION-001` Decision 9: retries physical deletion of every
/// input a prior compaction cycle deferred (its `Arc` strong count was
/// still `> 1` at the time) — never blocks; an input still referenced
/// simply stays deferred for the next call. A free function (Increment
/// 2) so both `LsmEngine::compact_once` and `compact_once_impl` itself
/// (called from the background worker) can share it without needing
/// `&LsmEngine`.
fn sweep_pending_compaction_deletes(pending_compaction_deletes: &Mutex<Vec<Arc<SsTable>>>) {
    let mut deferred = pending_compaction_deletes
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    deferred.retain(|input| {
        if Arc::strong_count(input) == 1 {
            let _ = std::fs::remove_file(input.path());
            false
        } else {
            true
        }
    });
}

/// `ADR-COMPACTION-001`'s deterministic core operation, extracted as a
/// free function in Increment 2 so both `LsmEngine::compact_once`
/// (passing `&self`'s own field `Arc`s) and the background compaction
/// worker thread (passing its own cloned field `Arc`s, since `LsmEngine
/// ::open` cannot hand a spawned thread `&LsmEngine`/`Arc<LsmEngine>`
/// — it returns a bare `Self`, exactly the same constraint `spawn_
/// flush_thread`'s own existing free-function-over-cloned-`Arc`s shape
/// already works around) can call the identical logic. Behavior is
/// byte-for-byte unchanged from Increment 1's own `compact_once`
/// method body, plus exactly one addition: `CompactionRunGuard`
/// (Increment 2 §4), acquired right after the trigger-count check so a
/// concurrent caller (manual or automatic) finds `Ok(None)` — "another
/// compaction is already in progress" is a legitimate skip reason,
/// exactly like "storage not Healthy" or "below trigger_count" already
/// were in Increment 1.
///
/// Sequence (per `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §7 /
/// `ADR-COMPACTION-001` Decisions 2/3/7/8/9/10/11):
/// 1. Observe (never mutate) `storage_state()` — skip while not
///    `Healthy` (Decision 11).
/// 2. Retry any previously-deferred physical deletions first (Decision
///    9's "next cycle retries") — runs even if a compaction is already
///    in progress elsewhere (Increment 2: this step never needs the
///    run guard, since it only ever touches already-retired tables no
///    in-progress cycle could also be touching).
/// 3. Acquire the run guard; below-trigger-count or another compaction
///    already running both yield `Ok(None)`.
/// 4. Brief read-lock capture of every live SSTable (§4) — released
///    immediately, before the merge.
/// 5. Stream the k-way merge (`compaction::merge`) straight into the
///    shared, generalized SSTable writer (`ADR-COMPACTION-001`
///    Decision 3) — no lock held during this, the expensive, part.
/// 6. Manifest `AddSstable`, fsync; then one `RemoveSstable` per input,
///    each fsync'd (Decision 7).
/// 7. One brief `sstables` write-lock: remove the input `Arc`s, insert
///    the output `Arc` (Decision 10/§13).
/// 8. Attempt physical deletion of each input whose `Arc` strong count
///    already allows it; defer (never block) the rest (Decision 9).
#[allow(clippy::too_many_arguments)]
fn compact_once_impl(
    sstables: &Arc<RwLock<Vec<Arc<SsTable>>>>,
    next_sstable_id: &Arc<AtomicU64>,
    sstables_dir: &Path,
    manifest: &Arc<Mutex<Manifest>>,
    compaction_trigger_count: usize,
    sstable_target_block_size: usize,
    bloom_bits_per_key: u32,
    snapshot_registry: &Arc<SnapshotRegistry>,
    storage_state: &Arc<AtomicU8>,
    compaction_fault_hook: &Arc<Mutex<Option<CompactionFaultHook>>>,
    compaction_io_fault_hook: &Arc<Mutex<Option<CompactionIoFaultHook>>>,
    pending_compaction_deletes: &Arc<Mutex<Vec<Arc<SsTable>>>>,
    compaction_running: &Arc<AtomicBool>,
    compaction_metrics: &Arc<CompactionMetricCounters>,
) -> Result<Option<(SstableMeta, CompactionStats)>> {
    let start = Instant::now();

    // Decision 11: observe only, never mutate `storage_state`.
    if StorageState::from_u8(storage_state.load(Ordering::Acquire)) != StorageState::Healthy {
        return Ok(None);
    }

    // Decision 9: retry deferred deletions before (not instead of)
    // attempting a new cycle -- independent of the run guard below,
    // since it only touches tables no in-progress cycle could also be
    // touching (they were already fully retired by a *previous* cycle).
    sweep_pending_compaction_deletes(pending_compaction_deletes);

    let sstable_count = sstables.read().unwrap_or_else(|p| p.into_inner()).len();
    if sstable_count < compaction_trigger_count {
        return Ok(None);
    }

    // Increment 2 §4: the smallest primitive preventing two
    // compactions (manual + automatic, or any other overlap) from
    // running at once. Held for the rest of this function via `_guard`
    // -- released automatically on every exit path, including `?`.
    let Some(_guard) = CompactionRunGuard::try_acquire(compaction_running) else {
        return Ok(None);
    };

    // §4: brief read-lock capture, released immediately.
    let inputs: Vec<Arc<SsTable>> = { sstables.read().unwrap_or_else(|p| p.into_inner()).clone() };
    if inputs.is_empty() {
        return Ok(None);
    }

    let input_sstable_count = inputs.len();
    let input_bytes: u64 = inputs
        .iter()
        .map(|t| std::fs::metadata(t.path()).map(|m| m.len()).unwrap_or(0))
        .sum();
    let entry_count_hint: u64 = inputs.iter().map(|t| t.record_count()).sum();
    let oldest_live_snapshot_seq = snapshot_registry.oldest();

    fire_compaction_fault_hook(
        compaction_fault_hook,
        CompactionFaultPoint::BeforeOutputWrite,
    );

    if let Some(e) = fire_compaction_io_fault_hook(compaction_io_fault_hook) {
        return Err(EngineError::Io(e));
    }

    let output_id = next_sstable_id.fetch_add(1, Ordering::SeqCst);
    let (records, merge_stats_handle) = crate::compaction::merge(&inputs, oldest_live_snapshot_seq);
    let writer_config = SsTableWriterConfig {
        target_block_size: sstable_target_block_size,
        bloom_bits_per_key,
    };
    let output_meta = sstable::write_from_sorted_records(
        records,
        output_id,
        sstables_dir,
        &writer_config,
        entry_count_hint,
    )?;
    let output_bytes = std::fs::metadata(&output_meta.path)
        .map(|m| m.len())
        .unwrap_or(0);
    let merge_stats = *merge_stats_handle.lock().unwrap_or_else(|p| p.into_inner());

    fire_compaction_fault_hook(
        compaction_fault_hook,
        CompactionFaultPoint::BeforeManifestAdd,
    );

    {
        let mut manifest = manifest.lock().unwrap_or_else(|p| p.into_inner());
        manifest.append_sync(ManifestEdit::AddSstable {
            id: output_id,
            min_seq: output_meta.min_seq,
            max_seq: output_meta.max_seq,
            file_size: output_bytes,
        })?;
    }

    fire_compaction_fault_hook(
        compaction_fault_hook,
        CompactionFaultPoint::AfterManifestAdd,
    );

    {
        let mut manifest = manifest.lock().unwrap_or_else(|p| p.into_inner());
        for input in &inputs {
            manifest.append_sync(ManifestEdit::RemoveSstable { id: input.id() })?;
            fire_compaction_fault_hook(
                compaction_fault_hook,
                CompactionFaultPoint::DuringRemoveSequence,
            );
        }
    }

    fire_compaction_fault_hook(compaction_fault_hook, CompactionFaultPoint::AfterAllRemoves);

    // §13: one brief write-lock — remove the input `Arc`s, insert the
    // output `Arc`. A concurrent flush's own splice (same `RwLock`) is
    // naturally serialized against this one; its own newly-published
    // table is never lost (it simply isn't among `inputs`, since it
    // was captured/published after this cycle's own input snapshot —
    // eligible for the *next* cycle).
    let output_table = Arc::new(SsTable::open(&output_meta.path, output_id)?);
    {
        let mut list = sstables.write().unwrap_or_else(|p| p.into_inner());
        let input_ids: HashSet<u64> = inputs.iter().map(|t| t.id()).collect();
        list.retain(|t| !input_ids.contains(&t.id()));
        if !list.iter().any(|t| t.id() == output_id) {
            list.insert(0, output_table);
        }
    }

    fire_compaction_fault_hook(
        compaction_fault_hook,
        CompactionFaultPoint::BeforePhysicalDelete,
    );

    // Decision 9: unlink each input only once its `Arc` strong count
    // has dropped to 1 (no reader — including this call's own now-
    // dropped `list` entry — still holds a clone); defer, never block,
    // on the rest.
    let mut deferred = pending_compaction_deletes
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for input in inputs {
        if Arc::strong_count(&input) == 1 {
            let _ = std::fs::remove_file(input.path());
        } else {
            deferred.push(input);
        }
    }
    drop(deferred);

    let records_dropped = merge_stats.records_read - merge_stats.records_retained;
    let stats = CompactionStats {
        input_sstable_count,
        output_sstable_count: 1,
        input_bytes,
        output_bytes,
        records_read: merge_stats.records_read,
        records_retained: merge_stats.records_retained,
        records_dropped,
        tombstones_dropped: merge_stats.tombstones_dropped,
        versions_dropped: merge_stats.versions_dropped,
        duration: start.elapsed(),
        peak_temp_disk_bytes: input_bytes + output_bytes,
    };

    record_compaction_cycle(compaction_metrics, &stats);

    Ok(Some((output_meta, stats)))
}

/// `ADR-COMPACTION-001` Increment 2 amendment: the background
/// compaction worker thread body. Mirrors `spawn_flush_thread`'s own
/// established shape (channel-driven, `stop`-flag-checked, cloned field
/// `Arc`s, no `&LsmEngine`) as closely as the two components' different
/// jobs allow. Two wake sources, unified into one code path: a real
/// `CompactionMsg::MaybeCompact` notification (sent by the flush thread
/// right after it publishes a new SSTable — see `spawn_flush_thread`'s
/// own body) and a periodic fallback tick (`recv_timeout`, reusing
/// `storage_pressure_retry_interval` as the cadence — no new config
/// field) so compaction still eventually retries even if write traffic
/// (and therefore flushes, and therefore `MaybeCompact` notifications)
/// stops entirely after a transient failure. Any additional queued
/// `MaybeCompact` messages are drained (coalesced) before acting, since
/// the channel itself is already bounded to capacity 1 and every wake
/// re-reads live state fresh regardless of how many notifications
/// prompted it. After one successful cycle, immediately re-checks
/// whether more work has already accumulated (a real, legitimate
/// catch-up loop, not a busy-loop: each iteration only continues
/// because a *real* completed compaction's own stats prove there was
/// real work to do) rather than waiting for a fresh external wake.
/// On `Err`, logs once (`ADR-COMPACTION-001` Increment 2 §8's own
/// documented retry policy: no internal busy-retry, no artificial
/// timer beyond the existing periodic fallback tick — wait for the
/// next real trigger or fallback tick, exactly like `Ok(None)`'s own
/// "nothing to do right now" outcome, just logged differently).
#[allow(clippy::too_many_arguments)]
fn spawn_compaction_thread(
    receiver: mpsc::Receiver<CompactionMsg>,
    sstables: Arc<RwLock<Vec<Arc<SsTable>>>>,
    next_sstable_id: Arc<AtomicU64>,
    sstables_dir: PathBuf,
    manifest: Arc<Mutex<Manifest>>,
    compaction_trigger_count: usize,
    sstable_target_block_size: usize,
    bloom_bits_per_key: u32,
    snapshot_registry: Arc<SnapshotRegistry>,
    storage_state: Arc<AtomicU8>,
    compaction_fault_hook: Arc<Mutex<Option<CompactionFaultHook>>>,
    compaction_io_fault_hook: Arc<Mutex<Option<CompactionIoFaultHook>>>,
    pending_compaction_deletes: Arc<Mutex<Vec<Arc<SsTable>>>>,
    compaction_running: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    fallback_interval: Duration,
    compaction_metrics: Arc<CompactionMetricCounters>,
) -> JoinHandle<()> {
    thread::spawn(move || loop {
        match receiver.recv_timeout(fallback_interval) {
            Ok(CompactionMsg::Shutdown) => return,
            Ok(CompactionMsg::MaybeCompact) => {
                // Coalesce: drain any further queued messages before
                // acting (the bounded(1) channel means there is at
                // most one more to drain in practice, but this loop is
                // correct regardless).
                loop {
                    match receiver.try_recv() {
                        Ok(CompactionMsg::Shutdown) => return,
                        Ok(CompactionMsg::MaybeCompact) => continue,
                        Err(_) => break,
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {} // periodic fallback wake
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
        // Catch-up loop: keep compacting while real work remains and
        // shutdown hasn't been requested.
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                compact_once_impl(
                    &sstables,
                    &next_sstable_id,
                    &sstables_dir,
                    &manifest,
                    compaction_trigger_count,
                    sstable_target_block_size,
                    bloom_bits_per_key,
                    &snapshot_registry,
                    &storage_state,
                    &compaction_fault_hook,
                    &compaction_io_fault_hook,
                    &pending_compaction_deletes,
                    &compaction_running,
                    &compaction_metrics,
                )
            }));
            match result {
                Ok(Ok(Some((meta, stats)))) => {
                    // §16: a normal successful compaction must be
                    // observable -- one concise, structured line, never
                    // repeated per threshold observation.
                    println!(
                        "compaction: succeeded output_id={} input_sstables={} \
                         output_bytes={} records_read={} records_retained={} \
                         records_dropped={} tombstones_dropped={} duration_ms={}",
                        meta.id,
                        stats.input_sstable_count,
                        stats.output_bytes,
                        stats.records_read,
                        stats.records_retained,
                        stats.records_dropped,
                        stats.tombstones_dropped,
                        stats.duration.as_millis(),
                    );
                    // Real catch-up: if enough new tables already
                    // accumulated during this cycle, go again
                    // immediately rather than waiting for another
                    // external wake.
                    let still_above_threshold =
                        sstables.read().unwrap_or_else(|p| p.into_inner()).len()
                            >= compaction_trigger_count;
                    if !still_above_threshold {
                        break;
                    }
                }
                Ok(Ok(None)) => break, // nothing to do right now
                Ok(Err(e)) => {
                    eprintln!("compaction: failed, will retry on the next trigger: {e}");
                    break;
                }
                Err(panic_payload) => {
                    let detail = panic_payload
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "non-string panic payload".to_string());
                    eprintln!("compaction: panicked, will retry on the next trigger: {detail}");
                    break;
                }
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_flush_thread(
    receiver: mpsc::Receiver<FlushMsg>,
    sstables: Arc<RwLock<Vec<Arc<SsTable>>>>,
    immutables: Arc<RwLock<VecDeque<Arc<MemTable>>>>,
    sstables_dir: PathBuf,
    next_sstable_id: Arc<AtomicU64>,
    writer_config: SsTableWriterConfig,
    max_retries: u32,
    stop: Arc<std::sync::atomic::AtomicBool>,
    delay_ms: Arc<AtomicU64>,
    pool: Arc<BatchCoordinatorPool>,
    manifest: Arc<Mutex<Manifest>>,
    checkpoint_seq: Arc<AtomicU64>,
    flush_fault_hook: Arc<Mutex<Option<FlushFaultHook>>>,
    flush_io_fault_hook: Arc<Mutex<Option<FlushIoFaultHook>>>,
    storage_state: Arc<AtomicU8>,
    storage_pressure_events: Arc<AtomicU64>,
    storage_pressure_retry_interval: Duration,
    compaction_sender: mpsc::SyncSender<CompactionMsg>,
    flush_completions: Arc<AtomicU64>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(msg) = receiver.recv() {
            let frozen = match msg {
                FlushMsg::Shutdown => break,
                FlushMsg::Flush(frozen) => frozen,
            };
            let mut attempt: u32 = 0;
            // Per-step idempotence tracking, not just "was the SSTable
            // built": a retry (triggered by a *later* step's failure)
            // must never redo an *earlier* step that already durably
            // succeeded. The first crash test run against this code
            // found exactly this bug: `pool.submit(CheckpointMarker)`
            // and the `SET_CHECKPOINT` manifest append were being
            // redone on every retry regardless of whether they had
            // already succeeded, producing duplicate durable marker
            // records for one logical flush (`PHASE5_ADR.md` has the
            // full account). `pool.rotate()` and `pool.purge_before()`
            // are deliberately *not* similarly guarded — both are
            // already idempotent/safe to repeat (existing, tested WAL
            // behavior; a redundant `rotate()` merely seals a
            // near-empty segment early, a redundant `purge_before` with
            // the same watermark is a no-op).
            let mut published: Option<(u64, crate::sstable::SstableMeta)> = None;
            let mut checkpoint_marker: Option<crate::wal::WalPosition> = None;
            let mut checkpoint_recorded = false;
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let delay = delay_ms.load(Ordering::Acquire);
                if delay > 0 {
                    sleep_checking_stop(Duration::from_millis(delay), &stop);
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                }

                let attempt_result: Result<()> =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let (id, meta) = match &published {
                            Some((id, meta)) => (*id, meta.clone()),
                            None => {
                                fire_flush_fault_hook(
                                    &flush_fault_hook,
                                    FlushFaultPoint::BeforeSstableWrite,
                                );
                                if let Some(injected) =
                                    fire_flush_io_fault_hook(&flush_io_fault_hook)
                                {
                                    return Err(EngineError::Io(injected));
                                }
                                let id = next_sstable_id.fetch_add(1, Ordering::SeqCst);
                                let meta = sstable::write_from_memtable(
                                    &frozen,
                                    id,
                                    &sstables_dir,
                                    &writer_config,
                                )?;
                                let file_size = std::fs::metadata(&meta.path)?.len();
                                manifest
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .append_sync(ManifestEdit::AddSstable {
                                        id,
                                        min_seq: meta.min_seq,
                                        max_seq: meta.max_seq,
                                        file_size,
                                    })?;
                                published = Some((id, meta.clone()));
                                fire_flush_fault_hook(
                                    &flush_fault_hook,
                                    FlushFaultPoint::AfterSstablePublish,
                                );
                                (id, meta)
                            }
                        };

                        {
                            let mut list = sstables.write().unwrap_or_else(|p| p.into_inner());
                            if !list.iter().any(|t| t.id() == id) {
                                let table = SsTable::open(&meta.path, id)?;
                                list.insert(0, Arc::new(table));
                            }
                        }
                        // `ADR-COMPACTION-001` Increment 2 §5/§16: a new
                        // SSTable just became live -- notify the
                        // compaction worker (if any) that `should_
                        // compact()` may now be worth re-checking.
                        // Best-effort, non-blocking, zero impact on this
                        // flush's own durability/timing contract either
                        // way: `try_send` never blocks (a full/absent
                        // channel just drops the notification -- the
                        // worker's own periodic fallback tick, or a
                        // later flush's own notification, covers it).
                        let _ = compaction_sender.try_send(CompactionMsg::MaybeCompact);

                        pool.rotate()?;
                        fire_flush_fault_hook(&flush_fault_hook, FlushFaultPoint::AfterRotate);
                        let position = match checkpoint_marker {
                            Some(position) => position,
                            None => {
                                let position = pool
                                    .submit(WalOpOwned::CheckpointMarker {
                                        flushed_through_seq: meta.max_seq,
                                    })?
                                    .wait()?;
                                checkpoint_marker = Some(position);
                                fire_flush_fault_hook(
                                    &flush_fault_hook,
                                    FlushFaultPoint::AfterCheckpointMarker,
                                );
                                position
                            }
                        };
                        if !checkpoint_recorded {
                            manifest
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .append_sync(ManifestEdit::SetCheckpoint {
                                    flushed_through_seq: meta.max_seq,
                                    wal_segment_id: position.segment_id,
                                    wal_offset: position.offset,
                                })?;
                            checkpoint_recorded = true;
                            fire_flush_fault_hook(
                                &flush_fault_hook,
                                FlushFaultPoint::AfterSetCheckpoint,
                            );
                        }
                        checkpoint_seq.store(meta.max_seq, Ordering::Release);
                        immutables
                            .write()
                            .unwrap_or_else(|p| p.into_inner())
                            .retain(|m| !Arc::ptr_eq(m, &frozen));
                        pool.purge_before(meta.max_seq)?;
                        Ok(())
                    }))
                    .unwrap_or_else(|panic_payload| {
                        Err(EngineError::Aborted {
                            detail: format!(
                                "flush attempt panicked: {}",
                                panic_payload_message(&panic_payload)
                            ),
                        })
                    });

                match attempt_result {
                    Ok(()) => {
                        // `ADR-WE-SP-001` §6.1/§13: any successful
                        // end-to-end flush is the one thing that clears
                        // storage-pressure state, regardless of which
                        // state it was in beforehand -- a real
                        // persistence success is stronger evidence of
                        // recovery than a free-space check could be.
                        // This deliberately collapses the ADR's
                        // StorageFull -> StoragePressure -> Healthy
                        // two-step description into one direct
                        // transition on genuine success (see
                        // `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md`
                        // implementation notes) -- the safety property
                        // that step exists for (never claim recovery
                        // without a real successful persistence attempt)
                        // is preserved exactly, since this branch only
                        // runs on that attempt's actual success.
                        let previous =
                            storage_state.swap(StorageState::Healthy as u8, Ordering::AcqRel);
                        if previous != StorageState::Healthy as u8 {
                            eprintln!(
                                "rubixdb: storage state {:?} -> HEALTHY (flush succeeded after \
                                 {attempt} failed attempt(s))",
                                StorageState::from_u8(previous)
                            );
                        }
                        // Test/observability-only counter, incremented
                        // strictly after every other side effect of a
                        // successful flush (publish, Manifest edits,
                        // immutable removal, WAL purge, the storage_state
                        // swap above) -- the one signal a waiter can poll
                        // to know a flush job has *fully* settled, not
                        // just that the memtable left `immutables`. Added
                        // to close a real, empirically-observed race:
                        // `immutable_count()==0` alone is observable
                        // *before* this job's tail (`purge_before` +
                        // the storage_state swap) has finished, so a
                        // test forcing `storage_state` right after that
                        // condition could have its write silently
                        // clobbered by this job's own delayed swap. Never
                        // read by production logic -- purely additive.
                        flush_completions.fetch_add(1, Ordering::Release);
                        break;
                    }
                    Err(e) => {
                        attempt += 1;
                        let is_enospc = e.is_storage_exhausted();
                        if is_enospc {
                            storage_pressure_events.fetch_add(1, Ordering::Relaxed);
                        }
                        if attempt <= max_retries {
                            // Bounded fast retry -- unchanged from the
                            // pre-ADR behavior for every error kind; by
                            // construction this phase is brief and
                            // bounded, so logging every attempt here
                            // does not reproduce the log-flood the ADR
                            // is about (`PHASE5_ENOSPC_FAILURE_ANALYSIS.md`
                            // §5/§8's 4,407-line stderr came entirely
                            // from attempts *past* this budget).
                            eprintln!(
                                "rubixdb: flush attempt {attempt} failed \
                                 (published={published:?}): {e}"
                            );
                            sleep_checking_stop(
                                Duration::from_millis(50u64.saturating_mul(attempt as u64)),
                                &stop,
                            );
                        } else if is_enospc {
                            // `ADR-WE-SP-001` §8: fast-retry budget
                            // exhausted on a *confirmed* ENOSPC failure
                            // -- exactly the condition the pre-ADR code
                            // treated identically to every other I/O
                            // error (flat 2s retry, forever, one log
                            // line per attempt). Enter STORAGE_PRESSURE,
                            // log the transition once rather than per
                            // attempt, and back off at the slower,
                            // configured cadence instead.
                            let transitioned = storage_state.compare_exchange(
                                StorageState::Healthy as u8,
                                StorageState::StoragePressure as u8,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            );
                            if transitioned.is_ok() {
                                eprintln!(
                                    "rubixdb: storage state HEALTHY -> STORAGE_PRESSURE (flush \
                                     attempt {attempt} failed with a storage-capacity error: {e})"
                                );
                            } else if attempt.is_multiple_of(10) {
                                // Aggregate heartbeat, not one line per
                                // attempt (`ADR-WE-SP-001` §14: "repeated
                                // identical errors must not flood logs").
                                eprintln!(
                                    "rubixdb: still in {:?} after {attempt} total flush \
                                     attempt(s) (most recent: {e})",
                                    StorageState::from_u8(storage_state.load(Ordering::Acquire))
                                );
                            }
                            sleep_checking_stop(storage_pressure_retry_interval, &stop);
                        } else {
                            // Non-ENOSPC I/O error past the fast-retry
                            // budget: out of `ADR-WE-SP-001`'s scope
                            // (targeted at storage-capacity exhaustion
                            // specifically -- see
                            // `PHASE5_ENOSPC_FAILURE_ANALYSIS.md` §6),
                            // so the retry cadence is unchanged from the
                            // pre-ADR flat 2s-forever behavior. Logging
                            // is still throttled, matching the same
                            // observability goal.
                            if attempt.is_multiple_of(10) {
                                eprintln!(
                                    "rubixdb: flush attempt {attempt} still failing (non-storage-\
                                     capacity error): {e}"
                                );
                            }
                            sleep_checking_stop(Duration::from_secs(2), &stop);
                        }
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                    }
                }
            }
        }
    })
}

/// Extracts a human-readable message from a caught panic payload —
/// `std::panic::catch_unwind`'s own `Err` type is `Box<dyn Any + Send>`
/// with no guaranteed structure; `panic!("...")`/`.unwrap()`-style
/// panics are the two shapes actually seen in practice.
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Sleeps for `total`, checking `stop` in small increments so a stuck
/// flush retry loop remains responsive to `shutdown` instead of blocking
/// it for up to the full backoff duration.
fn sleep_checking_stop(total: Duration, stop: &std::sync::atomic::AtomicBool) {
    const STEP: Duration = Duration::from_millis(50);
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let step = remaining.min(STEP);
        thread::sleep(step);
        remaining -= step;
    }
}

/// Applies one WAL-recovered record to a MemTable being reconstructed.
/// `CheckpointMarker` is still a no-op here as of Phase 5 — not because
/// checkpoints don't matter (they now do, deeply), but because the
/// checkpoint *value* used to decide what to replay at all comes from
/// the Manifest (`LsmEngine::open`'s own `checkpoint_at_replay` filter,
/// applied by the caller *before* this function is ever invoked for a
/// given record) — the marker record's own presence during replay
/// carries no additional information this function needs to act on.
fn apply_wal_op(memtable: &mut MemTable, seq: u64, op: WalOp<'_>) {
    match op {
        WalOp::Put { key, value } => memtable.put(key, seq, value),
        WalOp::Delete { key } => memtable.delete(key, seq),
        WalOp::CheckpointMarker { .. } => {}
    }
}

#[cfg(test)]
mod tests;
