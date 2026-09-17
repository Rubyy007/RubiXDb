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

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::{EngineError, Result};
use crate::execution::batch_coordinator::{BatchCoordinatorConfig, BatchCoordinatorPool};
use crate::manifest::{self, Manifest, ManifestEdit, ManifestState};
use crate::memtable::{MemTable, MemtableValue, DEFAULT_MAX_SIZE_BYTES};
use crate::sstable::{self, RecordValue, SsTable, SsTableWriterConfig};
use crate::wal::{self, FileWal, GroupCommitter, Wal, WalConfig, WalOp, WalOpOwned};

/// `RubixDB-LSM-Engine-Specification-v1.0.md` §4.1's `LsmConfig`, plus
/// the Phase-4B fields it also defines (`sstable_target_block_size`,
/// `bloom_bits_per_key`) — still no `compaction_trigger_count` (no
/// compaction this phase).
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
}

impl Default for LsmConfig {
    fn default() -> Self {
        LsmConfig {
            memtable_max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
            max_immutable_memtables: 4,
            sstable_target_block_size: crate::sstable::format::DEFAULT_TARGET_BLOCK_SIZE,
            bloom_bits_per_key: crate::sstable::format::DEFAULT_BLOOM_BITS_PER_KEY,
            max_flush_retries: 3,
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
    config: LsmConfig,
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
        let flush_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flush_delay_ms = Arc::new(AtomicU64::new(0));
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
        );

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
            config: lsm_config,
        })
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

    /// `Put(key, value)` — operating brief §12. Submits through the
    /// unmodified Dedicated Batch Coordinator, waits for WAL durability
    /// on the calling thread, then applies to the MemTable. Returns the
    /// assigned sequence number once fully durable *and* applied (i.e.
    /// once this call returns, both are guaranteed — `PHASE4A_
    /// ARCHITECTURE.md` §5's "logical completion" point).
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<u64> {
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
    /// refused, not the write that triggered it).
    fn freeze_locked(&self, active_guard: &mut MemTable) -> Result<()> {
        let mut immutables = self.lock_immutables_write();
        if immutables.len() >= self.config.max_immutable_memtables {
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
        {
            let active = self.lock_active_read();
            if let Some((_, value)) = active.get_as_of(key, as_of_seq) {
                return Ok(resolve(value));
            }
        }
        {
            let immutables = self.lock_immutables_read();
            for imm in immutables.iter() {
                if let Some((_, value)) = imm.get_as_of(key, as_of_seq) {
                    return Ok(resolve(value));
                }
            }
        }
        {
            let sstables = self.lock_sstables_read();
            for table in sstables.iter() {
                if let Some((_, value)) = table.get_versioned(key, as_of_seq)? {
                    return Ok(resolve_sstable(value));
                }
            }
        }
        Ok(None)
    }

    /// A snapshot boundary consistent with `PHASE4A_ARCHITECTURE.md` §5's
    /// ordering rule: only durable data is ever snapshot-visible. Callers
    /// wanting "as of right now, including whatever I just wrote" should
    /// use the `u64` this method returns immediately after their own
    /// `put`/`delete` call, not a value captured beforehand.
    pub fn snapshot_seq(&self) -> u64 {
        self.pool.stats().committer_stats.durable_through
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

                        pool.rotate()?;
                        let position = match checkpoint_marker {
                            Some(position) => position,
                            None => {
                                let position = pool
                                    .submit(WalOpOwned::CheckpointMarker {
                                        flushed_through_seq: meta.max_seq,
                                    })?
                                    .wait()?;
                                checkpoint_marker = Some(position);
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
                    Ok(()) => break,
                    Err(e) => {
                        attempt += 1;
                        eprintln!(
                            "rubixdb: flush attempt {attempt} failed \
                             (published={published:?}): {e}"
                        );
                        let backoff = if attempt <= max_retries {
                            Duration::from_millis(50u64.saturating_mul(attempt as u64))
                        } else {
                            Duration::from_secs(2)
                        };
                        sleep_checking_stop(backoff, &stop);
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
