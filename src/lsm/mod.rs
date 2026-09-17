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

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::{EngineError, Result};
use crate::execution::batch_coordinator::{BatchCoordinatorConfig, BatchCoordinatorPool};
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

        // No Manifest this phase (`PHASE4B_ADR.md` ADR-P4B-1): liveness is
        // "exists under `sstables/` and validates"
        // (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.3). A validation
        // failure here fails `open()` closed rather than silently
        // excluding the file (ADR-P4B-2) — safe to do because the WAL,
        // untouched by any flush, still holds every record regardless.
        let sstables_dir = dir.join("sstables");
        let (discovered_tables, next_id) = sstable::discover(&sstables_dir)?;

        let immutables = Arc::new(RwLock::new(VecDeque::new()));
        let sstables = Arc::new(RwLock::new(discovered_tables));
        let next_sstable_id = Arc::new(AtomicU64::new(next_id));

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
        );

        Ok(LsmEngine {
            pool,
            active: RwLock::new(active),
            immutables,
            sstables,
            next_sstable_id,
            sstables_dir,
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

/// The background flush thread body (`PHASE4B_ADR.md` ADR-P4B-5):
/// drains `immutables` oldest-first as messages arrive, building and
/// atomically publishing each one's SSTable
/// (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.4), then moving it from
/// `immutables` into `sstables`. Never calls `wal::purge_before` and
/// never touches the WAL (`PHASE4B_ADR.md` ADR-P4B-1). A failed flush
/// (`PHASE4B_ARCHITECTURE.md` §6) is retried with a short backoff for
/// `max_retries` attempts, then with a longer fixed backoff
/// indefinitely — the `ImmutableMemTable` is never dropped and never
/// silently abandoned; only `flush_stop` can end the loop early, and
/// only between backoff waits.
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
) -> JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(msg) = receiver.recv() {
            let frozen = match msg {
                FlushMsg::Shutdown => break,
                FlushMsg::Flush(frozen) => frozen,
            };
            let mut attempt: u32 = 0;
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
                let id = next_sstable_id.fetch_add(1, Ordering::SeqCst);
                let outcome =
                    sstable::write_from_memtable(&frozen, id, &sstables_dir, &writer_config)
                        .and_then(|meta| SsTable::open(&meta.path, meta.id));
                match outcome {
                    Ok(table) => {
                        sstables
                            .write()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(0, Arc::new(table));
                        immutables
                            .write()
                            .unwrap_or_else(|p| p.into_inner())
                            .retain(|m| !Arc::ptr_eq(m, &frozen));
                        break;
                    }
                    Err(e) => {
                        attempt += 1;
                        eprintln!("rubixdb: sstable flush attempt {attempt} (id {id}) failed: {e}");
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
