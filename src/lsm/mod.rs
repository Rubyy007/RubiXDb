//! LSM engine facade — Phase 4A scope only: write-path integration
//! (`Logical Client -> Dedicated Batch Coordinator -> Group Commit ->
//! Durable WAL -> Mutable MemTable -> Immutable MemTable`), per
//! `PHASE4A_ARCHITECTURE.md`. **Not** the full `RubixDB-LSM-Engine-
//! Specification-v1.0.md` §4 facade — no `sstables`/`manifest`/
//! `next_sstable_id` fields, no compaction, no flush-to-SSTable. Extended
//! (not replaced) in Phase 4B once RUBIC SSTable exists.
//!
//! # Durability
//!
//! This module introduces **no second durability system**. Every write
//! still goes through the unmodified `execution::batch_coordinator::
//! BatchCoordinatorPool` -> `wal::GroupCommitter` -> `wal::FileWal` path;
//! `MemTable::insert` is called only after `Completion::wait()` confirms
//! WAL durability (`PHASE4A_ARCHITECTURE.md` §5's exact ordering
//! contract). A crash before that point loses nothing this module is
//! responsible for recovering — the WAL already has it, and `LsmEngine::
//! open`'s own recovery step (`wal::replay_streaming`) reconstructs the
//! MemTable from it on restart.
//!
//! # Concurrency
//!
//! See `PHASE4A_MEMTABLE_ARCHITECTURE.md` §3 for the full account: the
//! active `MemTable` is guarded by `RwLock`, applied to by whichever
//! caller thread's own `Completion::wait()` returns first (not
//! exclusively the coordinator thread) — safe because `(user_key, seq)`
//! keys are globally unique, so concurrent inserts commute regardless of
//! application order.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::error::{EngineError, Result};
use crate::execution::batch_coordinator::{BatchCoordinatorConfig, BatchCoordinatorPool};
use crate::memtable::{MemTable, MemtableValue, DEFAULT_MAX_SIZE_BYTES};
use crate::wal::{self, FileWal, GroupCommitter, Wal, WalConfig, WalOp, WalOpOwned};

/// Phase-4A-scoped subset of `RubixDB-LSM-Engine-Specification-v1.0.md`
/// §4.1's `LsmConfig` — only the fields this phase actually uses.
#[derive(Debug, Clone)]
pub struct LsmConfig {
    /// LSM Engine Spec §4.1's own `memtable_max_size_bytes` default (4
    /// MiB) — see `memtable::DEFAULT_MAX_SIZE_BYTES`'s own doc comment
    /// for why this, not "~64 MiB," is the followed value.
    pub memtable_max_size_bytes: usize,
    /// Operating brief §21: bounded immutable-memtable backpressure.
    /// Phase 4A has no flush-to-SSTable path yet (Phase 4B), so
    /// immutables are never drained this phase — this bound is the one
    /// thing standing between sustained write load and unbounded memory
    /// growth, and is treated as load-bearing.
    pub max_immutable_memtables: usize,
}

impl Default for LsmConfig {
    fn default() -> Self {
        LsmConfig {
            memtable_max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
            max_immutable_memtables: 4,
        }
    }
}

/// A resolved read result — `Some(value)` for a live `Put`, `None` for
/// "not found" (no version at or before the query's `as_of_seq`, or the
/// newest visible version is a tombstone — operating brief §9/§14's own
/// collapse rule, applied once here across every source, not per-source).
pub type GetResult = Option<Vec<u8>>;

pub struct LsmEngine {
    pool: Arc<BatchCoordinatorPool>,
    active: RwLock<MemTable>,
    immutables: RwLock<VecDeque<Arc<MemTable>>>,
    config: LsmConfig,
}

impl LsmEngine {
    /// Opens (or creates) the WAL directory at `dir`, reconstructs the
    /// `GroupCommitter`/`BatchCoordinatorPool` exactly as any existing
    /// caller of those types already would (no change to that path), and
    /// recovers the MemTable via `wal::replay_streaming` — bounded
    /// memory, not `open_for_recovery`'s own full-materialization API
    /// (`PHASE4A_MEMTABLE_ARCHITECTURE.md` §10).
    pub fn open(
        dir: &Path,
        wal_config: WalConfig,
        pool_config: BatchCoordinatorConfig,
        lsm_config: LsmConfig,
    ) -> Result<Self> {
        let mut active = MemTable::new(lsm_config.memtable_max_size_bytes);

        // Read-only streaming pass, reconstructing the MemTable — must
        // happen *before* `FileWal::open_for_recovery` below takes the
        // directory's exclusive lock (this call only ever takes a shared
        // one, matching `inspect`'s own contract), so there is no lock
        // ordering conflict between the two.
        let _summary = wal::replay_streaming(dir, &wal_config, |seq, op| {
            apply_wal_op(&mut active, seq, op);
            Ok(())
        })?;

        let (file_wal, _replay) = FileWal::open_for_recovery(dir, wal_config)?;
        let committer = GroupCommitter::new(file_wal)?;
        let pool = Arc::new(BatchCoordinatorPool::new(committer, pool_config)?);

        Ok(LsmEngine {
            pool,
            active: RwLock::new(active),
            immutables: RwLock::new(VecDeque::new()),
            config: lsm_config,
        })
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
        let old = std::mem::replace(active_guard, fresh);
        immutables.push_front(old.freeze());
        Ok(())
    }

    /// `Get(key)` — latest visible value as of "now." Equivalent to
    /// `get_as_of(key, u64::MAX)`.
    pub fn get(&self, key: &[u8]) -> GetResult {
        self.get_as_of(key, u64::MAX)
    }

    /// `Get(key, snapshot_sequence)` — operating brief §14/§16. Consults
    /// the active MemTable first, then each immutable newest-to-oldest —
    /// active's own entries for any key are always sequence-higher than
    /// any immutable's (an immutable was always frozen strictly before
    /// the current active memtable began accumulating its own entries),
    /// so the first source with *any* version at `seq <= as_of_seq` is
    /// guaranteed authoritative; older sources are never consulted once
    /// a hit is found (mirroring LSM Engine Spec §4.2's own `get`
    /// algorithm, without the SSTable tier this phase does not have).
    pub fn get_as_of(&self, key: &[u8], as_of_seq: u64) -> GetResult {
        {
            let active = self.lock_active_read();
            if let Some((_, value)) = active.get_as_of(key, as_of_seq) {
                return resolve(value);
            }
        }
        let immutables = self.lock_immutables_read();
        for imm in immutables.iter() {
            if let Some((_, value)) = imm.get_as_of(key, as_of_seq) {
                return resolve(value);
            }
        }
        None
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

    /// Read-only access to the underlying pool's own stats — reuses the
    /// existing, unmodified observability surface rather than
    /// duplicating it.
    pub fn pool_stats(&self) -> crate::execution::batch_coordinator::BatchCoordinatorStats {
        self.pool.stats()
    }

    /// Delegates to the unmodified `BatchCoordinatorPool::shutdown` —
    /// this module introduces no new shutdown semantics of its own.
    pub fn shutdown(&self) -> crate::execution::batch_coordinator::ShutdownReportBC {
        self.pool.shutdown()
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
}

fn resolve(value: &MemtableValue) -> GetResult {
    match value {
        MemtableValue::Put(v) => Some(v.clone()),
        MemtableValue::Tombstone => None,
    }
}

/// Applies one WAL-recovered record to a MemTable being reconstructed —
/// `CheckpointMarker` is a no-op in Phase 4A (there is no flush-to-
/// SSTable/checkpoint concept yet; every durable WAL record belongs in
/// the recovered MemTable, full stop — `PHASE4A_MEMTABLE_ARCHITECTURE.md`
/// §10's own closing note).
fn apply_wal_op(memtable: &mut MemTable, seq: u64, op: WalOp<'_>) {
    match op {
        WalOp::Put { key, value } => memtable.put(key, seq, value),
        WalOp::Delete { key } => memtable.delete(key, seq),
        WalOp::CheckpointMarker { .. } => {}
    }
}

#[cfg(test)]
mod tests;
