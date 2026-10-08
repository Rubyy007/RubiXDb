//! SSTable integrity at the product layer (ADR-SST-01, F-08).
//!
//! Two things, both strictly above the engine (nothing under `src/sstable/`, `src/lsm/`, `src/manifest/`, `src/wal/`,
//! `src/compaction/` or `src/error.rs` changed; the engine's own public functions are called read-only):
//!
//! * [`preflight`] - a read-only check run by the startup-only guard (`ops::format::startup_guard_with_tail_policy`)
//!   **before** the F-07 attestation is consumed and before the engine opens. Every table the Manifest lists as live
//!   must exist, have the recorded size, open with the engine's own `SsTable::open` (footer, bloom, index) and carry
//!   the recorded sequence range; every other `*.sst` the engine would adopt must open. Anything else refuses the start
//!   with a message that names the file, and the directory is left byte-for-byte as it was.
//! * [`run_verification`] - the throttled, cancellable background pass that reads every data block of the tables that
//!   were live at start (the same scan `rubixdb check` performs) and records the first failing table's finding in
//!   [`SstableIntegrity`], which the API reports (`/readyz`, `/v1/status`). It never changes `ready` and takes no action.
//!
//! The engine already refuses everything it validates at open (`LsmEngine::open`); what it does not check is what the
//! preflight adds (the Manifest's record of a table, and refusing before any file is changed) and what the pass adds
//! (data blocks). `PHASE_ITEM_F08_ADR.md`.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::EngineError;
use crate::ops::{codes, OpsError};
use crate::sstable::{self, SsTable};

/// Default read budget of the background pass, in MiB per second. **An unvalidated proposal** (ADR-SST-01): its cost
/// to foreground latency under load has not been measured (`PHASE_ITEM_F08_IMPLEMENTATION.md`).
pub const DEFAULT_VERIFY_MIB_PER_SEC: u64 = 64;
/// Upper bound of the setting: above the measured one-core scan rate (677 MiB/s) there is nothing to gain.
pub const MAX_VERIFY_MIB_PER_SEC: u64 = 1024;

// --- A. the startup preflight -------------------------------------------------------------------------------------

/// A table the preflight validated; the background pass verifies exactly these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightTable {
    pub id: u64,
    pub path: PathBuf,
    /// The footer's record count.
    pub record_count: u64,
}

/// What the preflight saw (only returned when it did not refuse).
#[derive(Debug, Clone, Default)]
pub struct PreflightReport {
    /// The Manifest-live tables that were validated, in id order.
    pub tables: Vec<PreflightTable>,
    pub micros: u128,
}

const ACTION: &str =
    "Run `rubixdb check` and restore a verified backup into a new instance. The directory has not been modified.";

fn refuse(code: &'static str, detail: String, why: &str) -> OpsError {
    OpsError::new(code, format!("{detail}; refusing to open: {why}. {ACTION}"))
}

/// The engine's own message for a failed table open (`lsm::wrap_sstable_path_error`, which is private): the same
/// variants get the same `sstable <path>: ` prefix, so the text the operator sees is the text the engine would print.
fn engine_text(e: EngineError, path: &Path) -> String {
    match e {
        EngineError::Corruption { detail } => EngineError::Corruption {
            detail: format!("sstable {}: {detail}", path.display()),
        },
        EngineError::Unsupported { operation } => EngineError::Unsupported {
            operation: format!("sstable {}: {operation}", path.display()),
        },
        other => other,
    }
    .to_string()
}

const WHY_LIVE_CORRUPT: &str =
    "a table the Manifest lists as live is damaged and the engine would not serve a complete database";
const WHY_LIVE_MISSING: &str =
    "a table the Manifest lists as live is missing and the engine would not serve a complete database";
const WHY_LIVE_MISMATCH: &str = "a table the Manifest lists as live is not the file the Manifest recorded and the engine would serve a different database";
const WHY_UNRECORDED_CORRUPT: &str =
    "an unrecorded table in the sstables directory is damaged and the engine would refuse to open it";

/// Read-only; see the module docs. The directory is `<instance>/data`.
pub fn preflight(dir: &Path) -> Result<PreflightReport, OpsError> {
    let started = Instant::now();
    let replay = crate::manifest::replay_readonly(dir).map_err(|e| {
        OpsError::new(
            codes::ENGINE,
            format!("{e}; refusing to open. The directory has not been modified."),
        )
    })?;
    let state = replay.state;
    let sst_dir = dir.join("sstables");
    let mut tables = Vec::with_capacity(state.live_sstables.len());

    // 1-4: every Manifest-live table, in id order.
    for (&id, entry) in &state.live_sstables {
        let path = sst_dir.join(sstable::sstable_filename(id));
        let len = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(refuse(
                    codes::SSTABLE_MISSING,
                    format!(
                        "corruption: manifest: sstable id {id} is recorded live but {} is missing from disk",
                        path.display()
                    ),
                    WHY_LIVE_MISSING,
                ))
            }
            Err(e) => {
                return Err(refuse(
                    codes::SSTABLE_CORRUPT,
                    format!("sstable {}: {e}", path.display()),
                    WHY_LIVE_CORRUPT,
                ))
            }
        };
        // A recorded 0 means "unknown" (the engine records `unwrap_or(0)` if its own metadata call failed).
        if entry.file_size != 0 && len != entry.file_size {
            return Err(refuse(
                codes::SSTABLE_MISMATCH,
                format!(
                    "sstable {}: the file is {len} bytes but the Manifest recorded {}",
                    path.display(),
                    entry.file_size
                ),
                WHY_LIVE_MISMATCH,
            ));
        }
        let table = SsTable::open(&path, id).map_err(|e| {
            refuse(
                codes::SSTABLE_CORRUPT,
                engine_text(e, &path),
                WHY_LIVE_CORRUPT,
            )
        })?;
        if table.min_seq() != entry.min_seq || table.max_seq() != entry.max_seq {
            return Err(refuse(
                codes::SSTABLE_MISMATCH,
                format!(
                    "sstable {}: the file's sequence range is {}-{} but the Manifest recorded {}-{}",
                    path.display(),
                    table.min_seq(),
                    table.max_seq(),
                    entry.min_seq,
                    entry.max_seq
                ),
                WHY_LIVE_MISMATCH,
            ));
        }
        tables.push(PreflightTable {
            id,
            path,
            record_count: table.record_count(),
        });
    }

    // 5: every other published table the engine would adopt (neither live nor ever recorded) must open. `*.sst.tmp`
    // and removed-but-undeleted tables are the engine's to sweep.
    if sst_dir.is_dir() {
        let mut unrecorded: BTreeMap<u64, PathBuf> = BTreeMap::new();
        let entries = std::fs::read_dir(&sst_dir).map_err(|e| {
            OpsError::new(
                codes::IO,
                format!(
                    "cannot list {}: {e}; refusing to open. The directory has not been modified.",
                    sst_dir.display()
                ),
            )
        })?;
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Some(id) = sstable::parse_sstable_id(name) else {
                continue;
            };
            if !state.live_sstables.contains_key(&id) && !state.ever_added.contains(&id) {
                unrecorded.insert(id, entry.path());
            }
        }
        for (id, path) in unrecorded {
            SsTable::open(&path, id).map_err(|e| {
                refuse(
                    codes::SSTABLE_CORRUPT,
                    engine_text(e, &path),
                    WHY_UNRECORDED_CORRUPT,
                )
            })?;
        }
    }

    Ok(PreflightReport {
        tables,
        micros: started.elapsed().as_micros(),
    })
}

// --- B. the background verification ------------------------------------------------------------------------------

/// `disabled` (never started, or the setting is 0) | `running` (the pass is reading) | `complete` (every table read,
/// no finding) | `damaged` (at least one finding; set as soon as the first one is made, the pass may still be running -
/// `tables_verified < tables_total` says so).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationState {
    Disabled,
    Running,
    Complete,
    Damaged,
}

impl VerificationState {
    pub fn as_str(self) -> &'static str {
        match self {
            VerificationState::Disabled => "disabled",
            VerificationState::Running => "running",
            VerificationState::Complete => "complete",
            VerificationState::Damaged => "damaged",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => VerificationState::Running,
            2 => VerificationState::Complete,
            3 => VerificationState::Damaged,
            _ => VerificationState::Disabled,
        }
    }
}

/// One finding: the first failing read of one table. The engine's error carries no block offset and keys are never
/// recorded, so this is all that is known (and all that is ever reported).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DamagedTable {
    pub id: u64,
    /// `sstables/<file>`, relative to the data directory.
    pub path: String,
    pub records_before_failure: u64,
    /// The record count in the table's footer.
    pub records_total: u64,
}

impl DamagedTable {
    /// The one stderr line per damaged table: the file and the operator action; never row data or keys.
    pub fn stderr_line(&self) -> String {
        format!(
            "rubixdb: SSTABLE DAMAGED: {} failed verification after {} of {} records; the rows stored in the damaged block cannot be read. \
Run `rubixdb check`, then restore a verified backup into a new instance.",
            self.path, self.records_before_failure, self.records_total
        )
    }

    /// The security-log `object` field: ids and counts only.
    pub fn security_object(&self) -> String {
        format!(
            "table={} records_before={}",
            self.id, self.records_before_failure
        )
    }
}

/// A copy of the verification state for the reporting routes.
#[derive(Debug, Clone)]
pub struct IntegritySnapshot {
    pub state: VerificationState,
    pub tables_total: u64,
    /// Tables whose scan has ended - clean, damaged, or skipped as retired by a compaction - so
    /// `tables_verified == tables_total` means the pass has finished.
    pub tables_verified: u64,
    pub bytes_verified: u64,
    pub damaged: Vec<DamagedTable>,
}

/// Shared state of the background pass, owned by `AppState`. Nothing is persisted.
#[derive(Debug, Default)]
pub struct SstableIntegrity {
    state: AtomicU8,
    cancel: AtomicBool,
    tables_total: AtomicU64,
    tables_verified: AtomicU64,
    bytes_verified: AtomicU64,
    damaged: Mutex<Vec<DamagedTable>>,
}

impl SstableIntegrity {
    pub fn state(&self) -> VerificationState {
        VerificationState::from_u8(self.state.load(Ordering::SeqCst))
    }

    pub fn snapshot(&self) -> IntegritySnapshot {
        IntegritySnapshot {
            state: self.state(),
            tables_total: self.tables_total.load(Ordering::SeqCst),
            tables_verified: self.tables_verified.load(Ordering::SeqCst),
            bytes_verified: self.bytes_verified.load(Ordering::SeqCst),
            damaged: self
                .damaged
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
        }
    }

    /// Asks a running pass to stop at its next block. Idempotent; harmless when nothing runs.
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Marks the pass as running over `tables_total` tables. Called before the thread is spawned so a request can never
    /// observe a stale `disabled`.
    pub fn begin(&self, tables_total: u64) {
        self.tables_total.store(tables_total, Ordering::SeqCst);
        self.tables_verified.store(0, Ordering::SeqCst);
        self.bytes_verified.store(0, Ordering::SeqCst);
        self.damaged
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        self.state
            .store(VerificationState::Running as u8, Ordering::SeqCst);
    }

    /// Back to `disabled` (the pass could not be started).
    pub fn disable(&self) {
        self.state
            .store(VerificationState::Disabled as u8, Ordering::SeqCst);
    }

    fn record_damage(&self, d: DamagedTable) {
        self.damaged
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(d);
        self.state
            .store(VerificationState::Damaged as u8, Ordering::SeqCst);
    }

    fn finish(&self) {
        let damaged = !self
            .damaged
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty();
        let s = if damaged {
            VerificationState::Damaged
        } else {
            VerificationState::Complete
        };
        self.state.store(s as u8, Ordering::SeqCst);
    }
}

/// Bytes-per-second pacing against an absolute clock, so sleep overshoot is repaid rather than accumulated.
struct Pace {
    bytes_per_sec: u64,
    started: Instant,
    consumed: u64,
}

impl Pace {
    fn new(mib_per_sec: u64) -> Self {
        Pace {
            bytes_per_sec: mib_per_sec.max(1) * 1024 * 1024,
            started: Instant::now(),
            consumed: 0,
        }
    }

    /// Accounts `bytes` and sleeps (in slices of at most 20 ms, checking `cancel`) until the budget allows them.
    fn take(&mut self, bytes: u64, cancel: &AtomicBool) {
        self.consumed = self.consumed.saturating_add(bytes);
        let due = Duration::from_secs_f64(self.consumed as f64 / self.bytes_per_sec as f64);
        loop {
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            let elapsed = self.started.elapsed();
            if elapsed >= due {
                return;
            }
            std::thread::sleep((due - elapsed).min(Duration::from_millis(20)));
        }
    }
}

/// Bytes accumulated before the pace is consulted: small enough to keep the rate smooth, large enough not to sleep per
/// block (a 4 KiB block at 64 MiB/s is 61 microseconds).
const PACE_STEP_BYTES: u64 = 256 * 1024;

/// Reads every data block of `tables` once, at most `mib_per_sec` MiB per second, stopping at the next block when
/// `integrity.cancel_requested()`. A table that has disappeared (a compaction retired it) is skipped. The first error
/// of a table ends that table's scan and is recorded and passed to `on_damaged`. The caller has already called
/// [`SstableIntegrity::begin`].
pub fn run_verification(
    integrity: &SstableIntegrity,
    data_dir: &Path,
    tables: &[PreflightTable],
    mib_per_sec: u64,
    on_damaged: &mut dyn FnMut(&DamagedTable),
) {
    let mut pace = Pace::new(mib_per_sec);
    for t in tables {
        if integrity.cancel_requested() {
            return;
        }
        let rel = t
            .path
            .strip_prefix(data_dir)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| format!("sstables/{}", sstable::sstable_filename(t.id)));
        let table = match SsTable::open(&t.path, t.id) {
            Ok(table) => table,
            Err(EngineError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                // Retired by a compaction since the preflight.
                integrity.tables_verified.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            Err(_) => {
                // Validated by the preflight, no longer valid: the table was damaged after the start.
                finding(integrity, &rel, t, 0, on_damaged);
                continue;
            }
        };
        let file_len = std::fs::metadata(&t.path).map(|m| m.len()).unwrap_or(0);
        let per_block = file_len / (table.block_count().max(1) as u64);
        let mut records = 0u64;
        let mut blocks_seen = 0u64;
        let mut pending = 0u64;
        let mut failed = false;
        for item in table.range_scan_raw(Bound::Unbounded, Bound::Unbounded) {
            if integrity.cancel_requested() {
                return;
            }
            match item {
                Ok(_) => records += 1,
                Err(_) => {
                    finding(integrity, &rel, t, records, on_damaged);
                    failed = true;
                    break;
                }
            }
            let blocks = table.blocks_read();
            if blocks > blocks_seen {
                let bytes = (blocks - blocks_seen) * per_block;
                blocks_seen = blocks;
                integrity.bytes_verified.fetch_add(bytes, Ordering::SeqCst);
                pending += bytes;
                if pending >= PACE_STEP_BYTES {
                    pace.take(pending, &integrity.cancel);
                    pending = 0;
                }
            }
        }
        if pending > 0 {
            pace.take(pending, &integrity.cancel);
        }
        if !failed {
            integrity.tables_verified.fetch_add(1, Ordering::SeqCst);
        }
    }
    if !integrity.cancel_requested() {
        integrity.finish();
    }
}

fn finding(
    integrity: &SstableIntegrity,
    rel: &str,
    t: &PreflightTable,
    records_before: u64,
    on_damaged: &mut dyn FnMut(&DamagedTable),
) {
    let d = DamagedTable {
        id: t.id,
        path: rel.to_string(),
        records_before_failure: records_before,
        records_total: t.record_count,
    };
    integrity.record_damage(d.clone());
    integrity.tables_verified.fetch_add(1, Ordering::SeqCst);
    on_damaged(&d);
}

/// `RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC`: digits only; `None` (unset or empty) is the default; `0` disables the
/// pass; the upper bound is [`MAX_VERIFY_MIB_PER_SEC`]. The environment variable name lives with the other
/// `RUBIXDB_LOCAL_*` settings in `cli/src/startup_env.rs`.
pub fn parse_verify_mib_per_sec(var: &str, v: Option<&str>) -> Result<u64, String> {
    let Some(v) = v else {
        return Ok(DEFAULT_VERIFY_MIB_PER_SEC);
    };
    let bad = || {
        format!(
            "{var} must be an integer between 0 (disabled) and {MAX_VERIFY_MIB_PER_SEC}, got {v:?}"
        )
    };
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n = v.parse::<u64>().map_err(|_| bad())?;
    if n > MAX_VERIFY_MIB_PER_SEC {
        return Err(bad());
    }
    Ok(n)
}
