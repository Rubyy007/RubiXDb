//! Write-ahead log. Implements `RubixDB-WAL-Specification-v1.0.md`: the
//! 24-byte segment header and CRC32C-framed records (§2), sequence number
//! assignment (§3), the `Immediate`-mode durability boundary (§4), the
//! `Wal` trait and `inspect()` (§5), the torn-write-vs-corruption recovery
//! rule (§6, with one deliberate amendment — see "# Durability" below),
//! segment rotation (§7), and segment retention (§10).
//!
//! # Safety (thread and process model)
//!
//! `FileWal` is a **single-writer** type: `append`/`sync`/`rotate`/
//! `purge_before` all take `&mut self`, and internal bookkeeping
//! (`sealed_max_seq`, `active_segment_has_records`, the active segment's
//! own byte offset) assumes exactly one logical writer is ever in flight.
//! `FileWal` is `Send` (it may be handed off to another thread entirely)
//! but is deliberately **not** `Sync` (enforced by a `PhantomData<Cell<()>>`
//! marker field, checked at compile time by a `static_assertions` test in
//! this module) — sharing `&FileWal` across threads and relying on the
//! type system to make that safe was never the design, and a future
//! refactor that accidentally made every field `Sync` must not silently
//! change that contract. To use one `FileWal` from multiple threads,
//! wrap it in a mutex:
//!
//! ```ignore
//! use std::sync::{Arc, Mutex};
//! let wal = Arc::new(Mutex::new(file_wal));
//! // from any thread:
//! wal.lock().unwrap().append_sync(op)?;
//! ```
//!
//! The single-writer invariant is not just documented, and does not stop
//! at this process's boundary: `open_for_recovery` takes an OS-level
//! exclusive advisory lock on the WAL directory (`file_io::
//! acquire_exclusive_lock`, `std::fs::File::try_lock` — `flock` on Unix,
//! `LockFileEx` on Windows), held for the `FileWal`'s entire lifetime and
//! released automatically when it's dropped or if this process dies. A
//! second `open_for_recovery` on the same directory — from a genuinely
//! separate process, or an unsynchronized second call within this one —
//! fails immediately with `EngineError::WalUnavailable` rather than
//! racing the first call's directory scan, torn-tail truncation, and
//! segment writes. `inspect` takes a compatible *shared* lock (so any
//! number of `inspect` calls may run concurrently with each other, but
//! never while a writer holds the exclusive one), and takes no lock at
//! all — silently, correctly — against a directory no writer has ever
//! opened yet (see `file_io::acquire_shared_lock_if_present`'s doc
//! comment).
//!
//! # Durability
//!
//! - **Directory fsync** (Unix only — see `file_io::fsync_dir`'s doc
//!   comment for the Windows gap): after a segment file is created
//!   (`create_new_segment_file`) and after each segment file removed by
//!   `purge_before`, the containing directory is fsynced so the
//!   create/unlink itself — not just the file's own contents — survives a
//!   crash.
//! - **Poison on unrecoverable write failure**: `SegmentIo::append`
//!   (`file_io.rs`) rolls a failed write back to the pre-append length; if
//!   that rollback itself fails, the segment is poisoned and refuses
//!   further I/O rather than risk writing on top of an unknown-length
//!   file (Group 1.1).
//! - **Fail-closed recovery, amended**: WAL Spec §6.2 originally allowed
//!   scanning to continue past a corrupted (non-last) segment so later,
//!   intact segments could still contribute records. This implementation
//!   now stops at the **first** corrupted segment and trusts nothing
//!   after it — including that segment's own records that happened to
//!   precede the corruption point within it. Once any segment's integrity
//!   is in question, nothing after it in the WAL's logical order can be
//!   trusted to be positioned correctly relative to still-undiscovered
//!   damage, so the conservative choice is to stop rather than assemble a
//!   partial picture that looks more complete than it safely is. This is
//!   a deliberate amendment to the spec text, not an oversight — see
//!   `scan_directory`'s doc comment.
//! - **`inspect()` is genuinely read-only**: it neither creates the WAL
//!   directory if absent (`canonicalize_existing_dir` — Group 4.1) nor
//!   opens segment files for writing (`scan_segment`'s `mutate` parameter
//!   — Group 4.2), so it can be run against a read-only directory.
//! - **`SyncMode::GroupCommit` is rejected, not silently downgraded**:
//!   `open_for_recovery` returns `EngineError::Unsupported` if configured
//!   with `GroupCommit`, since no batching implementation exists yet
//!   (Group 5.2) — a caller can never silently get `Immediate` behavior
//!   while believing it configured something else.
//! - **Purge failure tolerance.** If `purge_before` fails midway, the
//!   batch's directory fsync is still attempted; if that also fails, some
//!   unlinked entries may resurrect after a crash. This is tolerated:
//!   every purged segment's records are already durable elsewhere (WAL
//!   Spec §10), so recovery re-sees a sealed segment containing only
//!   already-applied data. The resurrection is a space cost, never a
//!   correctness cost — but it is a second line of defense, not a
//!   substitute for actually attempting the fsync whenever the directory
//!   genuinely changed, which `purge_before` does unconditionally
//!   whenever at least one segment was removed, including on its own
//!   error path.

mod file_io;
mod format;
#[cfg(test)]
mod fuzz_tests;
mod ops;
mod recovery;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;

pub use file_io::WalFile;
pub use format::{DEFAULT_MAX_RECORD_LEN, DEFAULT_MAX_SEGMENT_SIZE};
pub use ops::{WalOp, WalOpOwned};

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{EngineError, Result};
use file_io::{
    acquire_exclusive_lock, acquire_shared_lock_if_present, fsync_dir, SegmentIo, LOCK_FILE_NAME,
};
use format::{encode_segment_header, SEGMENT_HEADER_LEN};
use recovery::walk_full_segment;

/// WAL Spec §5.
#[derive(Debug, Clone)]
pub struct WalConfig {
    pub max_record_len: usize,
    pub max_segment_size: u64,
    pub sync_mode: SyncMode,
}

impl Default for WalConfig {
    fn default() -> Self {
        WalConfig {
            max_record_len: DEFAULT_MAX_RECORD_LEN,
            max_segment_size: DEFAULT_MAX_SEGMENT_SIZE,
            sync_mode: SyncMode::Immediate,
        }
    }
}

/// WAL Spec §4.2. `GroupCommit`'s batching behavior is explicitly
/// post-Phase-0 — the variant exists so the config surface is forward-
/// compatible, but no `Wal` implementation in this crate honors it yet:
/// `FileWal::open_for_recovery` returns `EngineError::Unsupported` if a
/// `WalConfig` configured with `GroupCommit` is passed to it (Group 5.2),
/// rather than silently running in `Immediate` mode as an earlier version
/// of this module did — a caller opting into "batching" must be told
/// there is none yet, not quietly handed different behavior than it
/// asked for.
#[derive(Debug, Clone, Copy)]
pub enum SyncMode {
    Immediate,
    GroupCommit {
        max_wait: Duration,
        max_batch_bytes: usize,
    },
}

/// WAL Spec §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WalPosition {
    pub segment_id: u64,
    pub offset: u64,
    pub seq: u64,
}

/// WAL Spec §5. `corrupted_segments` carries the *IDs* of every segment
/// where §6.3 corruption (not a torn tail) was found; per the WAL Spec's
/// own text, a non-empty list here means the caller must not proceed to
/// `ACTIVE` (Architecture Spec §3.1's `DEGRADED`). As of Group 3.1, scans
/// stop at the first corrupted segment (see this module's "# Durability"
/// section), so this `Vec` never has more than one entry in practice —
/// kept as a `Vec` rather than narrowed to `Option<u64>` so the type
/// doesn't need to change again if a future revision wants to resume
/// collecting further corruption sites for diagnostic purposes.
#[derive(Debug, Default)]
pub struct WalReplayResult {
    pub records: Vec<(u64, WalOpOwned)>,
    pub last_valid_position: WalPosition,
    pub truncated: bool,
    pub corrupted_segments: Vec<u64>,
}

/// WAL Spec §5.
pub trait Wal: Sized {
    fn open_for_recovery(dir: &Path, config: WalConfig) -> Result<(Self, WalReplayResult)>;
    fn append(&mut self, op: WalOp) -> Result<WalPosition>;
    fn sync(&mut self) -> Result<()>;

    fn append_sync(&mut self, op: WalOp) -> Result<WalPosition> {
        let pos = self.append(op)?;
        self.sync()?;
        Ok(pos)
    }

    fn rotate(&mut self) -> Result<()>;
    fn purge_before(&mut self, watermark_seq: u64) -> Result<Vec<u64>>;
    fn current_segment_id(&self) -> u64;
    fn next_seq(&self) -> u64;
}

const SEGMENT_FILE_PREFIX: &str = "wal-";
const SEGMENT_FILE_SUFFIX: &str = ".log";
const SEGMENT_ID_DIGITS: usize = 20;

fn segment_file_name(id: u64) -> String {
    format!(
        "{SEGMENT_FILE_PREFIX}{id:0width$}{SEGMENT_FILE_SUFFIX}",
        width = SEGMENT_ID_DIGITS
    )
}

fn parse_segment_file_name(name: &str) -> Option<u64> {
    let digits = name
        .strip_prefix(SEGMENT_FILE_PREFIX)?
        .strip_suffix(SEGMENT_FILE_SUFFIX)?;
    if digits.len() != SEGMENT_ID_DIGITS || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// The next segment ID after `id`, or `CapacityExceeded` if `id` is
/// already `u64::MAX` (Group 3.4) — `u64::MAX` records is not a limit
/// this format expects to hit in practice, but a checked arithmetic
/// operation must never silently wrap regardless of how implausible the
/// overflow is, per the Non-Negotiable Security bar.
fn next_segment_id(id: u64) -> Result<u64> {
    id.checked_add(1).ok_or(EngineError::CapacityExceeded {
        requested: u64::MAX,
        max: u64::MAX,
    })
}

/// Canonicalizes `dir`, creating it first if absent — used by
/// `open_for_recovery`, which is allowed to bring a fresh WAL directory
/// into existence. See `canonicalize_existing_dir` for `inspect`'s
/// read-only counterpart (Group 4.1).
fn canonicalize_data_dir(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let canonical = dir.canonicalize()?;
    Ok(canonical)
}

/// Canonicalizes `dir` **without** creating it — `inspect`'s directory
/// resolution (Group 4.1). Returns `EngineError::NotFound` specifically
/// when the directory is absent, distinct from other I/O errors (e.g.
/// permission denied), so a caller can tell "there is no WAL here yet"
/// apart from "something is wrong trying to read it."
fn canonicalize_existing_dir(dir: &Path) -> Result<PathBuf> {
    match dir.canonicalize() {
        Ok(p) => Ok(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(EngineError::NotFound),
        Err(e) => Err(e.into()),
    }
}

fn segment_path(canonical_dir: &Path, id: u64) -> PathBuf {
    canonical_dir.join(segment_file_name(id))
}

fn lock_file_path(canonical_dir: &Path) -> PathBuf {
    canonical_dir.join(LOCK_FILE_NAME)
}

fn list_segment_ids(canonical_dir: &Path) -> Result<Vec<u64>> {
    let mut ids = Vec::new();
    for entry in fs::read_dir(canonical_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if let Some(id) = parse_segment_file_name(name) {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// Creates and fully initializes a new segment file: opens it exclusively
/// (`create_new`, so this can never silently overwrite an existing
/// segment), writes and fsyncs its 24-byte header, then fsyncs the
/// containing directory (Group 1.2).
///
/// Atomic w.r.t. partial failure (Group 1.3): if the header write/fsync
/// fails, the just-created (partially-initialized) file is removed
/// best-effort before returning the original error, so a segment file
/// never exists on disk without a valid header — recovery would otherwise
/// treat it as corruption on the next open. If that cleanup removal also
/// fails, both errors are folded into the one returned `EngineError` so
/// neither is silently dropped.
fn create_new_segment_file(path: &Path, id: u64, canonical_dir: &Path) -> Result<File> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)?;

    // Group 1.3: every failure from here on (header write, header fsync,
    // *or* the directory fsync) must clean up the just-created file the
    // same way — "any failure after create_new(true)" per spec, not just
    // a header-write failure specifically. `init_result` folds all three
    // fallible steps into one `Result` so exactly one cleanup path
    // handles all of them.
    let init_result = file
        .write_all(&encode_segment_header(id))
        .and_then(|()| file.sync_all())
        .and_then(|()| fsync_dir(canonical_dir));

    if let Err(init_err) = init_result {
        drop(file);
        return Err(match fs::remove_file(path) {
            Ok(()) => init_err.into(),
            Err(remove_err) => EngineError::Io(io::Error::new(
                init_err.kind(),
                format!(
                    "segment {id} initialization failed ({init_err}) and removing the \
                     partially-created file also failed ({remove_err})"
                ),
            )),
        });
    }

    fire_abort_hook(AbortPoint::AfterHeader);
    Ok(file)
}

/// The outcome of scanning one segment during the directory-wide walk in
/// `scan_directory`. `outcome` is `None` if the header itself failed
/// validation — always corruption regardless of position (WAL Spec §6.2
/// step 1); the specific `EngineError` is not retained because
/// `WalReplayResult.corrupted_segments` (Tier 1: fixed as `Vec<u64>`) has
/// nowhere to carry it — the segment ID alone is what the spec's contract
/// promises callers. `mutate` controls whether the segment is opened for
/// writing at all (Group 4.2): `inspect` (`mutate = false`) never opens a
/// segment file with write access, so it can run against a read-only WAL
/// directory.
struct SegmentScan {
    file: File,
    file_len: u64,
    outcome: Option<recovery::WalkOutcome>,
}

fn scan_segment(
    canonical_dir: &Path,
    id: u64,
    max_record_len: usize,
    mutate: bool,
) -> Result<SegmentScan> {
    let path = segment_path(canonical_dir, id);
    let mut file = OpenOptions::new().read(true).write(mutate).open(&path)?;
    let file_len = file.metadata()?.len();
    let outcome = walk_full_segment(&mut file, file_len, id, max_record_len)?.ok();
    Ok(SegmentScan {
        file,
        file_len,
        outcome,
    })
}

/// Shared implementation for `open_for_recovery` (`mutate = true`: torn
/// tails are physically truncated, per WAL Spec §6.4) and `inspect`
/// (`mutate = false`: read-only, per WAL Spec §5's doc comment on
/// `inspect`, strengthened by Group 4.2 so *no* segment file is ever
/// opened for writing on this path).
///
/// Returns the aggregate `WalReplayResult`; the highest contained `seq`
/// per sealed (non-active) segment that recovery could fully vouch for
/// (WAL Spec §10's `purge_before` input — a segment absent from this map,
/// because it was corrupted or never reached, is never purged); and, when
/// `mutate` is true, the open `File` handle and metadata for the segment
/// future appends should target.
///
/// **Stops at the first corrupted segment (Group 3.1, amending WAL Spec
/// §6.2's "keep scanning past a corrupt non-last segment" text — see this
/// module's "# Durability" section for why).** A segment contributes to
/// `result.records`/`sealed_max_seq`/`last_valid_position` only if it is
/// itself fully trusted (`outcome.corrupted == false`, and not a
/// too-early "torn tail"); the moment a segment fails either check, its
/// own (even partially valid) records are discarded, `corrupted_segments`
/// gets exactly that one ID, and the scan ends — nothing at or after that
/// segment is examined further. `last_valid_position` therefore only ever
/// reflects a record from a segment that was fully trusted (Group 3.2).
fn scan_directory(
    canonical_dir: &Path,
    config: &WalConfig,
    mutate: bool,
) -> Result<(
    WalReplayResult,
    BTreeMap<u64, u64>,
    Option<ActiveSegmentState>,
)> {
    let ids = list_segment_ids(canonical_dir)?;

    if ids.is_empty() {
        return Ok((
            WalReplayResult {
                records: Vec::new(),
                last_valid_position: WalPosition {
                    segment_id: 1,
                    offset: SEGMENT_HEADER_LEN as u64,
                    seq: 0,
                },
                truncated: false,
                corrupted_segments: Vec::new(),
            },
            BTreeMap::new(),
            None, // caller creates segment 1 fresh
        ));
    }

    let last_id = *ids.last().expect("ids is non-empty, checked above");
    let mut result = WalReplayResult {
        records: Vec::new(),
        last_valid_position: WalPosition {
            segment_id: last_id,
            offset: SEGMENT_HEADER_LEN as u64,
            seq: 0,
        },
        truncated: false,
        corrupted_segments: Vec::new(),
    };
    let mut sealed_max_seq = BTreeMap::new();
    let mut active_state: Option<ActiveSegmentState> = None;

    // NOTE(phase-0): this loop stops at the first corrupted segment by
    // design (Group 3.1 — see this module's "# Durability" section). A
    // future `inspect_all()` for operator diagnostics — which would walk
    // past corruption and report each segment's state plus explicit seq
    // gaps — is a *different* function with a *different* contract
    // ("what is on disk", not "what can be replayed"). It must not be
    // implemented by loosening this loop.
    for id in ids {
        let is_last = id == last_id;
        let scan = scan_segment(canonical_dir, id, config.max_record_len, mutate)?;

        let Some(outcome) = scan.outcome else {
            // Header itself failed validation — always corruption,
            // regardless of position (WAL Spec §6.2 step 1). Group 3.1:
            // stop here; nothing after this segment is trusted.
            result.corrupted_segments.push(id);
            break;
        };

        let non_tail_torn = outcome.truncated && !is_last;
        if outcome.corrupted || non_tail_torn {
            // A torn-tail *shape* in a non-last segment is not a
            // legitimate torn write (WAL Spec §6.3: only the very last
            // segment can be torn — every earlier segment was fsynced and
            // sealed by rotation before the next one was created); treat
            // it as corruption. Either way (real corruption, or this),
            // Group 3.1/3.2: this segment's own records are discarded too
            // — not trusted just because they preceded the failure point
            // within it — and the scan stops here.
            result.corrupted_segments.push(id);
            break;
        }

        // Only reached for a segment that is fully trusted: either
        // entirely clean, or (only if `is_last`) legitimately torn at its
        // very tail.
        if let Some(last) = outcome.records.last() {
            result.last_valid_position = WalPosition {
                segment_id: id,
                offset: outcome.valid_end_offset,
                seq: last.0,
            };
        }
        let this_segment_max_seq = outcome.records.last().map(|(s, _)| *s).unwrap_or(0);
        result.records.extend(outcome.records);

        if !is_last {
            sealed_max_seq.insert(id, this_segment_max_seq);
        }

        if is_last {
            if outcome.truncated {
                result.truncated = true;
                if mutate {
                    scan.file.set_len(outcome.valid_end_offset)?;
                    scan.file.sync_all()?;
                    // No `fsync_dir` here on purpose: `set_len` mutates
                    // only the file's inode metadata (its size field), and
                    // `sync_all` above flushes exactly that. Directory
                    // fsync is needed when a *directory entry* is
                    // created, renamed, or unlinked — none of which this
                    // path does (the entry already exists; nothing about
                    // the directory itself changes). Adding one here
                    // would be a no-op on every mainstream filesystem and
                    // pure cost on the recovery hot path.
                }
            }
            let mut file = scan.file;
            file.seek(SeekFrom::Start(if outcome.truncated {
                outcome.valid_end_offset
            } else {
                scan.file_len
            }))?;
            active_state = Some(ActiveSegmentState {
                id,
                file,
                size: if outcome.truncated {
                    outcome.valid_end_offset
                } else {
                    scan.file_len
                },
            });
        }
    }

    // If the last segment's *header* was unreadable, or scanning stopped
    // early on an earlier corrupted segment (Group 3.1), there is no
    // valid active segment to resume appending into. This is not directly
    // addressed by the WAL Spec's happy-path text (it only says
    // corrupted_segments must stop the caller from reaching ACTIVE); as a
    // Tier 2 implementation choice (doesn't change on-disk format, the
    // public API, or a correctness guarantee — the caller is already
    // required to halt on non-empty corrupted_segments regardless of what
    // FileWal does internally here), a fresh segment is opened after the
    // highest known ID so the returned `Wal` is at least structurally
    // usable by an operator tool after manual remediation.
    if active_state.is_none() && mutate {
        let new_id = next_segment_id(last_id)?;
        let path = segment_path(canonical_dir, new_id);
        let file = create_new_segment_file(&path, new_id, canonical_dir)?;
        active_state = Some(ActiveSegmentState {
            id: new_id,
            file,
            size: SEGMENT_HEADER_LEN as u64,
        });
    }

    Ok((result, sealed_max_seq, active_state))
}

struct ActiveSegmentState {
    id: u64,
    file: File,
    size: u64,
}

/// The production `Wal` implementation: real segment files under
/// `<data_dir>/wal/`. See this module's "# Safety" and "# Durability"
/// sections for the thread-safety and crash-consistency invariants this
/// type upholds.
#[derive(Debug)]
pub struct FileWal {
    canonical_dir: PathBuf,
    config: WalConfig,
    active_id: u64,
    active: SegmentIo<File>,
    next_seq: u64,
    /// Sealed (non-active) segments' highest contained `seq`, used by
    /// `purge_before` (WAL Spec §10). A segment absent from this map is
    /// never purged — the safe default for anything recovery couldn't
    /// fully vouch for (a corrupted segment, or one this process hasn't
    /// sealed itself yet).
    sealed_max_seq: BTreeMap<u64, u64>,
    /// Whether the *current* active segment contains at least one record
    /// (either recovered from before a crash, or appended since). Used
    /// only by `rotate()` to record an accurate `sealed_max_seq` entry for
    /// the segment being sealed: `next_seq - 1` is only that segment's
    /// true max `seq` when it actually has records — an active segment
    /// that was rotated away with zero appends (e.g., two `rotate()` calls
    /// back-to-back) must not be recorded as if it held the *previous*
    /// segment's last `seq`.
    active_segment_has_records: bool,
    /// This process's exclusive advisory lock on the WAL directory
    /// (`file_io::acquire_exclusive_lock`), held for as long as this
    /// `FileWal` lives and released automatically when it's dropped. This
    /// is what actually *enforces* the single-writer-per-directory
    /// invariant against a completely separate process — see this
    /// module's "# Safety" section.
    _lock: File,
    /// Deliberately `!Sync` marker — see this module's "# Safety" section.
    _not_sync: PhantomData<std::cell::Cell<()>>,
}

impl FileWal {
    fn wal_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("wal")
    }

    /// `true` if the active segment is poisoned (Group 1.1) and can no
    /// longer accept appends or syncs. A poisoned `FileWal` must be
    /// discarded and its directory recovered fresh via
    /// `open_for_recovery` — there is no in-process repair.
    pub fn is_poisoned(&self) -> bool {
        self.active.is_poisoned()
    }

    /// Installs the Group 7.2 crash-consistency test hook — see
    /// `AbortPoint` and `tests/crash_consistency.rs`. Only compiled with
    /// the `test-util` feature; a normal build has no such call available
    /// and pays nothing for it.
    #[cfg(feature = "test-util")]
    pub fn set_abort_hook(hook: fn(AbortPoint)) {
        abort_hook::set(hook);
    }
}

impl Wal for FileWal {
    fn open_for_recovery(dir: &Path, config: WalConfig) -> Result<(Self, WalReplayResult)> {
        if !matches!(config.sync_mode, SyncMode::Immediate) {
            // Group 5.2: never silently run GroupCommit-configured callers
            // in Immediate mode — see the module doc comment.
            return Err(EngineError::Unsupported {
                operation: "SyncMode::GroupCommit (WAL Spec §4.2): no batching \
                            implementation exists yet"
                    .to_string(),
            });
        }

        let canonical_dir = canonicalize_data_dir(&Self::wal_dir(dir))?;
        // Group "no file locking": acquired *before* any scan/mutation,
        // so a second concurrent `open_for_recovery` (another process, or
        // an unsynchronized second call within this one) fails here
        // rather than racing the directory listing, torn-tail truncation,
        // and segment creation below against this call's own.
        let lock = acquire_exclusive_lock(&lock_file_path(&canonical_dir))?;
        let (result, sealed_max_seq, active_state) = scan_directory(&canonical_dir, &config, true)?;

        let (active_id, active_size, file) = match active_state {
            Some(s) => (s.id, s.size, s.file),
            None => {
                let file =
                    create_new_segment_file(&segment_path(&canonical_dir, 1), 1, &canonical_dir)?;
                (1, SEGMENT_HEADER_LEN as u64, file)
            }
        };

        let next_seq = result.records.last().map(|(seq, _)| seq + 1).unwrap_or(1);
        let active_segment_has_records = active_size > SEGMENT_HEADER_LEN as u64;

        let wal = FileWal {
            canonical_dir,
            config,
            active_id,
            active: SegmentIo::new(file, active_size),
            next_seq,
            sealed_max_seq,
            active_segment_has_records,
            _lock: lock,
            _not_sync: PhantomData,
        };
        Ok((wal, result))
    }

    fn append(&mut self, op: WalOp) -> Result<WalPosition> {
        let seq = self.next_seq;
        let frame = ops::encode_wal_frame(seq, op, self.config.max_record_len)?;

        // WAL Spec §3.4: rotate only protects a segment that already has
        // content from growing past `max_segment_size` — a single record
        // whose framed size alone exceeds it is still written whole into
        // a (necessarily oversized) fresh segment, never split, and never
        // bounced into yet another empty segment first.
        let frame_total = self.active.size() + frame.len() as u64;
        if self.active_segment_has_records && frame_total > self.config.max_segment_size {
            self.rotate()?;
        }

        let offset = self.active.append(&frame)?;
        self.active_segment_has_records = true;
        self.next_seq += 1;
        fire_abort_hook(AbortPoint::MidAppend);
        Ok(WalPosition {
            segment_id: self.active_id,
            offset,
            seq,
        })
    }

    fn sync(&mut self) -> Result<()> {
        fire_abort_hook(AbortPoint::BeforeSync);
        self.active.sync()?;
        fire_abort_hook(AbortPoint::AfterSync);
        Ok(())
    }

    fn rotate(&mut self) -> Result<()> {
        // Group 1.4: create the new segment file *first*. If this fails,
        // nothing about `self` has been touched at all — `rotate()`
        // behaves as if it were never called.
        let new_id = next_segment_id(self.active_id)?;
        let new_path = segment_path(&self.canonical_dir, new_id);
        let new_file = create_new_segment_file(&new_path, new_id, &self.canonical_dir)?;

        // Only now attempt to seal the old active segment. WAL segments
        // are discovered purely by directory listing (there is no
        // Manifest-equivalent yet to have recorded the new file), so if
        // this fails, the just-created file is safe to delete and `self`
        // is left exactly as it was before this call.
        if let Err(sync_err) = self.active.sync() {
            let _ = fs::remove_file(&new_path);
            return Err(sync_err.into());
        }

        let sealed_max_seq = if self.active_segment_has_records {
            self.next_seq.saturating_sub(1)
        } else {
            0
        };
        self.sealed_max_seq.insert(self.active_id, sealed_max_seq);
        self.active_id = new_id;
        self.active = SegmentIo::new(new_file, SEGMENT_HEADER_LEN as u64);
        self.active_segment_has_records = false;
        Ok(())
    }

    fn purge_before(&mut self, watermark_seq: u64) -> Result<Vec<u64>> {
        let mut removed = Vec::new();
        let eligible: Vec<u64> = self
            .sealed_max_seq
            .iter()
            .filter(|(&id, &max_seq)| id != self.active_id && max_seq < watermark_seq)
            .map(|(&id, _)| id)
            .collect();

        // One file at a time; `sealed_max_seq`/`removed` are only ever
        // updated for a file whose removal has already succeeded, so a
        // failure partway through never leaves this map claiming a
        // deleted file still exists, nor vice versa.
        //
        // Track the first removal failure but still fsync whatever did
        // get removed — the "directory changed → fsync the directory"
        // rule (this module's "# Durability" section) applies on the
        // error path too, not just the happy path. It's a second line of
        // defense (every purged segment's data is already durable
        // elsewhere, so a lost fsync only risks a harmless resurrection,
        // never corruption — see "# Durability" below), not a substitute
        // for actually attempting the fsync whenever the directory
        // genuinely changed.
        let mut first_remove_err: Option<io::Error> = None;
        for id in eligible {
            let path = segment_path(&self.canonical_dir, id);
            match fs::remove_file(&path) {
                Ok(()) => {
                    self.sealed_max_seq.remove(&id);
                    removed.push(id);
                }
                Err(e) => {
                    first_remove_err = Some(e);
                    break;
                }
            }
        }
        removed.sort_unstable();

        // Fsync unconditionally if *any* entry was unlinked, even on
        // failure — batched once after the whole run rather than after
        // each removal (every file removed here was already
        // independently safe to lose per WAL Spec §10, so batching trades
        // a marginally later durability point for one syscall instead of
        // N; see "# Durability" below for the full argument).
        let fsync_result = if removed.is_empty() {
            Ok(())
        } else {
            fsync_dir(&self.canonical_dir)
        };

        // Fold both potential failures into one error so neither is lost.
        match (first_remove_err, fsync_result) {
            (None, Ok(())) => Ok(removed),
            (Some(e), Ok(())) => Err(e.into()),
            (None, Err(e)) => Err(e.into()),
            (Some(remove_err), Err(fsync_err)) => Err(EngineError::Io(io::Error::other(format!(
                "purge_before: remove failed ({remove_err}); directory fsync also failed \
                 ({fsync_err}); {} segment(s) were removed before the failure: {removed:?}",
                removed.len()
            )))),
        }
    }

    fn current_segment_id(&self) -> u64 {
        self.active_id
    }

    fn next_seq(&self) -> u64 {
        self.next_seq
    }
}

/// WAL Spec §5: read-only inspection, never mutates the directory (Group
/// 4: never creates it if absent, never opens a segment file for writing).
pub fn inspect(dir: &Path, config: &WalConfig) -> Result<WalReplayResult> {
    let canonical_dir = canonicalize_existing_dir(&FileWal::wal_dir(dir))?;
    // Group "no file locking": a shared lock, compatible with any number
    // of concurrent `inspect` calls but incompatible with an active
    // writer's exclusive lock — `FileWal::append` writes directly with no
    // atomic-rename step, so without this, `inspect` could genuinely
    // observe a segment file mid-write. `Ok(None)` (no lock file yet, no
    // writer has ever opened this directory) is not an error and takes no
    // lock at all — `inspect` must never create anything.
    let _lock = acquire_shared_lock_if_present(&lock_file_path(&canonical_dir))?;
    let (result, _sealed_max_seq, _active_state) = scan_directory(&canonical_dir, config, false)?;
    Ok(result)
}

/// A point in `FileWal`'s write path a crash-consistency test can ask to
/// abort at (Group 7.2 — see `tests/crash_consistency.rs`). Defined
/// unconditionally (it's a zero-cost enum) so call sites throughout this
/// module never need their own `#[cfg(feature = "test-util")]` gating;
/// only the hook *storage and dispatch* below are feature-gated, and
/// `fire_abort_hook` is a no-op when the feature is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortPoint {
    /// Immediately after a new segment's header has been written, fsynced,
    /// and the containing directory fsynced.
    AfterHeader,
    /// Immediately after a record's bytes have been written (via
    /// `SegmentIo::append`) but before `sync()` is called.
    MidAppend,
    /// Immediately before `sync()`'s `fsync` call.
    BeforeSync,
    /// Immediately after `sync()`'s `fsync` call completes.
    AfterSync,
}

#[cfg(feature = "test-util")]
mod abort_hook {
    use super::AbortPoint;
    use std::sync::OnceLock;

    static HOOK: OnceLock<fn(AbortPoint)> = OnceLock::new();

    /// Installs the process-wide abort hook. Intended to be called once,
    /// at the very start of a child test process, before any `FileWal`
    /// use — see `tests/crash_consistency.rs`. A second call is a no-op
    /// (the first hook wins); this is test harness code, not something
    /// production call sites need to reason about re-entrancy for.
    pub(super) fn set(hook: fn(AbortPoint)) {
        let _ = HOOK.set(hook);
    }

    pub(super) fn fire(point: AbortPoint) {
        if let Some(hook) = HOOK.get() {
            hook(point);
        }
    }
}

/// Calls the installed abort hook (if any) — a no-op when `test-util` is
/// disabled, so every call site in this module can call it unconditionally
/// without its own `#[cfg(...)]`.
fn fire_abort_hook(point: AbortPoint) {
    #[cfg(feature = "test-util")]
    abort_hook::fire(point);
    #[cfg(not(feature = "test-util"))]
    let _ = point;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::file_io::DirFsyncHook;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn segment_file_name_round_trip() {
        let name = segment_file_name(42);
        assert_eq!(name, "wal-00000000000000000042.log");
        assert_eq!(parse_segment_file_name(&name), Some(42));
    }

    #[test]
    fn parse_rejects_non_matching_names() {
        assert_eq!(parse_segment_file_name("wal-1.log"), None); // wrong width
        assert_eq!(
            parse_segment_file_name("notwal-00000000000000000001.log"),
            None
        );
        assert_eq!(
            parse_segment_file_name("wal-00000000000000000001.tmp"),
            None
        );
        assert_eq!(
            parse_segment_file_name("wal-0000000000000000000a.log"),
            None
        );
    }

    #[test]
    fn next_segment_id_rejects_overflow() {
        let err = next_segment_id(u64::MAX).unwrap_err();
        assert!(matches!(err, EngineError::CapacityExceeded { .. }));
        assert_eq!(next_segment_id(41).unwrap(), 42);
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("rubixdb_wal_modtest_{tag}_{nanos}_{n}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// Regression test for the missing-file-locking gap: a second
    /// `open_for_recovery` on the same directory while the first is still
    /// alive must fail fast (never block, never silently proceed), and a
    /// third attempt after the first is dropped (lock released) must
    /// succeed.
    #[test]
    fn concurrent_open_for_recovery_is_rejected_until_the_first_is_dropped() {
        let dir = temp_dir("lock_exclusive");
        let (wal1, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();

        let err = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap_err();
        assert!(
            matches!(err, EngineError::WalUnavailable { .. }),
            "expected WalUnavailable, got {err:?}"
        );

        drop(wal1);
        let (_wal2, _) = FileWal::open_for_recovery(&dir, WalConfig::default())
            .expect("lock must be released once the first FileWal is dropped");

        let _ = fs::remove_dir_all(&dir);
    }

    /// `inspect` must never create a lock file (or anything else) against
    /// a WAL directory no writer has ever touched, and must succeed
    /// freely there.
    #[test]
    fn inspect_succeeds_with_no_writer_ever_having_opened_the_directory() {
        let dir = temp_dir("lock_inspect_untouched");
        fs::create_dir_all(FileWal::wal_dir(&dir)).unwrap();
        let result = inspect(&dir, &WalConfig::default()).unwrap();
        assert!(result.records.is_empty());
        assert!(
            !lock_file_path(&FileWal::wal_dir(&dir).canonicalize().unwrap()).exists(),
            "inspect must never create the lock file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `inspect` must fail (not hang, not silently read a possibly-
    /// mid-write file) while a writer holds the exclusive lock, and must
    /// succeed again once that writer is gone. Two concurrent `inspect`
    /// calls (shared-shared) must both succeed.
    #[test]
    fn inspect_is_excluded_by_an_active_writer_but_not_by_another_inspect() {
        let dir = temp_dir("lock_inspect_vs_writer");
        let (wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();

        let err = inspect(&dir, &WalConfig::default()).unwrap_err();
        assert!(matches!(err, EngineError::WalUnavailable { .. }));

        drop(wal);
        inspect(&dir, &WalConfig::default()).expect("inspect must succeed once the writer is gone");

        // Two readers concurrently: both must succeed (shared-shared).
        let canonical = FileWal::wal_dir(&dir).canonicalize().unwrap();
        let lock1 = acquire_shared_lock_if_present(&lock_file_path(&canonical))
            .unwrap()
            .expect("lock file exists by now");
        let lock2 = acquire_shared_lock_if_present(&lock_file_path(&canonical))
            .unwrap()
            .expect("a second shared lock must not conflict with the first");
        drop(lock1);
        drop(lock2);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Group 5.2 regression test.
    #[test]
    fn group_commit_is_rejected_not_silently_downgraded() {
        let dir = temp_dir("group_commit");
        let config = WalConfig {
            sync_mode: SyncMode::GroupCommit {
                max_wait: Duration::from_millis(1),
                max_batch_bytes: 1,
            },
            ..WalConfig::default()
        };
        let err = FileWal::open_for_recovery(&dir, config).unwrap_err();
        assert!(matches!(err, EngineError::Unsupported { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Group 4.1 regression test: `inspect` must not create a missing WAL
    /// directory.
    #[test]
    fn inspect_never_creates_a_missing_directory() {
        let dir = temp_dir("inspect_no_create");
        fs::remove_dir_all(&dir).unwrap(); // exists (from temp_dir) then removed
        let err = inspect(&dir, &WalConfig::default()).unwrap_err();
        assert!(matches!(err, EngineError::NotFound));
        assert!(!dir.exists(), "inspect must not have created it");
    }

    /// Group 3.1/3.2 regression test: with segments `[1 valid, 2 corrupt,
    /// 3 valid]`, only segment 1's records are trusted, `corrupted_segments
    /// == [2]`, segment 3 is never even scanned, and `last_valid_position`
    /// points into segment 1.
    #[test]
    fn scan_stops_at_first_corruption_and_ignores_everything_after() {
        let dir = temp_dir("stop_at_corruption");
        {
            let (mut wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
            wal.append_sync(WalOp::Put {
                key: b"k1",
                value: b"v1",
            })
            .unwrap();
            wal.rotate().unwrap();
            wal.append_sync(WalOp::Put {
                key: b"k2",
                value: b"v2",
            })
            .unwrap();
            wal.rotate().unwrap();
            wal.append_sync(WalOp::Put {
                key: b"k3",
                value: b"v3",
            })
            .unwrap();
        }

        let wal_dir = FileWal::wal_dir(&dir);
        let mut ids = list_segment_ids(&wal_dir).unwrap();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3]);

        // Corrupt segment 2's magic.
        let seg2_path = segment_path(&wal_dir, 2);
        let mut f = OpenOptions::new().write(true).open(&seg2_path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"XXXXXXXX").unwrap();

        let (_, result) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        assert_eq!(result.corrupted_segments, vec![2]);
        assert_eq!(result.records.len(), 1);
        assert_eq!(
            result.records[0].1,
            WalOpOwned::Put {
                key: b"k1".to_vec(),
                value: b"v1".to_vec()
            }
        );
        assert_eq!(result.last_valid_position.segment_id, 1);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Group 1.2 regression test: a directory-fsync failure during
    /// `rotate()` must surface as `Err` and leave `FileWal`'s state
    /// exactly as it was before the call (Group 1.4's atomicity, exercised
    /// via the dir-fsync failure specifically).
    #[test]
    fn rotate_surfaces_dir_fsync_failure_and_leaves_state_unchanged() {
        let dir = temp_dir("rotate_dir_fsync_fail");
        let (mut wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        wal.append_sync(WalOp::Put {
            key: b"k",
            value: b"v",
        })
        .unwrap();

        let active_id_before = wal.active_id;
        let next_seq_before = wal.next_seq;
        let has_records_before = wal.active_segment_has_records;
        let sealed_before = wal.sealed_max_seq.clone();

        {
            let _guard =
                DirFsyncHook::install(|_path| Err(io::Error::other("injected dir fsync failure")));
            let err = wal.rotate().unwrap_err();
            assert!(matches!(err, EngineError::Io(_)));
        }

        assert_eq!(wal.active_id, active_id_before);
        assert_eq!(wal.next_seq, next_seq_before);
        assert_eq!(wal.active_segment_has_records, has_records_before);
        assert_eq!(wal.sealed_max_seq, sealed_before);
        // The would-be new segment file must not have been left behind.
        let leftover = segment_path(&FileWal::wal_dir(&dir), active_id_before + 1);
        assert!(!leftover.exists());

        // And the WAL must still be fully usable afterward.
        wal.rotate().unwrap();
        assert_eq!(wal.active_id, active_id_before + 1);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Group 1.2 regression test: same, for `purge_before`.
    #[test]
    fn purge_before_surfaces_dir_fsync_failure_and_leaves_state_consistent() {
        let dir = temp_dir("purge_dir_fsync_fail");
        let (mut wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        wal.append_sync(WalOp::Put {
            key: b"k",
            value: b"v",
        })
        .unwrap();
        wal.rotate().unwrap();
        wal.append_sync(WalOp::Put {
            key: b"k2",
            value: b"v2",
        })
        .unwrap();

        let sealed_before = wal.sealed_max_seq.clone();
        assert!(!sealed_before.is_empty());

        {
            let _guard =
                DirFsyncHook::install(|_path| Err(io::Error::other("injected dir fsync failure")));
            let err = wal.purge_before(u64::MAX).unwrap_err();
            assert!(matches!(err, EngineError::Io(_)));
        }

        // The files ARE gone (removal itself succeeded; only the final
        // directory fsync was injected to fail) — `sealed_max_seq` must
        // accurately reflect that, per Group 1.5's one-at-a-time update
        // rule, even though the call as a whole returned `Err`.
        for id in sealed_before.keys() {
            assert!(
                !wal.sealed_max_seq.contains_key(id),
                "segment {id} was removed from disk and must not still be tracked"
            );
            let path = segment_path(&FileWal::wal_dir(&dir), *id);
            assert!(!path.exists());
        }

        let _ = fs::remove_dir_all(&dir);
    }

    /// `purge_before` regression test: a `remove_file` failure on its own
    /// (directory fsync never even needed, since nothing was removed
    /// before hitting it) must surface as a plain `Io` error, and the
    /// segment whose removal failed must remain tracked in
    /// `sealed_max_seq` (Group 1.5's "only update on success" rule) even
    /// though the file itself is gone from disk — deleted out from under
    /// the WAL here specifically to trigger a deterministic
    /// `remove_file` failure without needing a dedicated fault-injection
    /// seam (unlike `fsync_dir`, `std::fs::remove_file` has no test hook;
    /// simulating "someone/something else already removed the file" is
    /// the simplest way to make `remove_file` itself fail predictably).
    #[test]
    fn purge_before_surfaces_a_remove_failure_alone() {
        let dir = temp_dir("purge_remove_fail_alone");
        let (mut wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        wal.append_sync(WalOp::Put {
            key: b"k1",
            value: b"v1",
        })
        .unwrap();
        wal.rotate().unwrap(); // seals segment 1
        wal.append_sync(WalOp::Put {
            key: b"k2",
            value: b"v2",
        })
        .unwrap();
        wal.rotate().unwrap(); // seals segment 2
        wal.append_sync(WalOp::Put {
            key: b"k3",
            value: b"v3",
        })
        .unwrap();
        // active is now segment 3; sealed_max_seq has entries for 1 and 2.
        assert_eq!(wal.sealed_max_seq.len(), 2);

        let seg1_path = segment_path(&FileWal::wal_dir(&dir), 1);
        fs::remove_file(&seg1_path).unwrap();

        let err = wal.purge_before(u64::MAX).unwrap_err();
        assert!(
            matches!(err, EngineError::Io(_)),
            "expected a plain Io error, got {err:?}"
        );

        // Segment 1 (lowest id, attempted first) failed immediately, so
        // nothing was removed before the failure; segment 1 stays
        // tracked (its removal was never recorded as successful) and
        // segment 2 was never even attempted.
        assert!(wal.sealed_max_seq.contains_key(&1));
        assert!(wal.sealed_max_seq.contains_key(&2));
        let seg2_path = segment_path(&FileWal::wal_dir(&dir), 2);
        assert!(
            seg2_path.exists(),
            "segment 2 must never have been attempted"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// `purge_before` regression test: when a `remove_file` failure
    /// happens *after* at least one segment was already removed, and the
    /// batch's directory fsync (still attempted, per this fix) also
    /// fails, both failures must be folded into one error whose message
    /// mentions both — neither is silently dropped in favor of the
    /// other.
    #[test]
    fn purge_before_combined_remove_and_fsync_failure_mentions_both() {
        let dir = temp_dir("purge_remove_and_fsync_fail");
        let (mut wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        wal.append_sync(WalOp::Put {
            key: b"k1",
            value: b"v1",
        })
        .unwrap();
        wal.rotate().unwrap(); // seals segment 1
        wal.append_sync(WalOp::Put {
            key: b"k2",
            value: b"v2",
        })
        .unwrap();
        wal.rotate().unwrap(); // seals segment 2
        wal.append_sync(WalOp::Put {
            key: b"k3",
            value: b"v3",
        })
        .unwrap();
        wal.rotate().unwrap(); // seals segment 3
        wal.append_sync(WalOp::Put {
            key: b"k4",
            value: b"v4",
        })
        .unwrap();
        // active is now segment 4; sealed_max_seq has entries for 1, 2, 3.

        // Segment 2 (not the first attempted) is removed externally, so
        // segment 1's removal succeeds before purge_before's own removal
        // of segment 2 fails.
        let seg2_path = segment_path(&FileWal::wal_dir(&dir), 2);
        fs::remove_file(&seg2_path).unwrap();

        let err = {
            let _guard =
                DirFsyncHook::install(|_path| Err(io::Error::other("injected dir fsync failure")));
            wal.purge_before(u64::MAX).unwrap_err()
        };

        match err {
            EngineError::Io(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("remove failed"),
                    "message must mention the remove failure: {msg}"
                );
                assert!(
                    msg.contains("fsync"),
                    "message must mention the fsync failure: {msg}"
                );
                assert!(
                    msg.contains('1'),
                    "message must mention which segment(s) were removed before \
                     the failure: {msg}"
                );
            }
            other => panic!("expected Io, got {other:?}"),
        }

        // Segment 1 succeeded (gone from disk, untracked); segment 2
        // failed (gone from disk — removed externally — but still
        // tracked, since purge_before's own removal of it never
        // succeeded); segment 3 was never attempted.
        assert!(!wal.sealed_max_seq.contains_key(&1));
        assert!(wal.sealed_max_seq.contains_key(&2));
        assert!(wal.sealed_max_seq.contains_key(&3));
        let seg1_path = segment_path(&FileWal::wal_dir(&dir), 1);
        assert!(!seg1_path.exists());
        let seg3_path = segment_path(&FileWal::wal_dir(&dir), 3);
        assert!(
            seg3_path.exists(),
            "segment 3 must never have been attempted"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // Group 5.1: compile-time proof `FileWal` is `Send` but not `Sync`.
    // `static_assertions` is a dev-dependency (per the user's explicit
    // instruction), so this assertion — like the crate itself has no
    // access to dev-dependencies outside `#[cfg(test)]` code — lives here
    // rather than at module scope. If a future refactor ever made every
    // field of `FileWal` `Sync` (its current fields all happen to be),
    // `FileWal` would silently become `Sync` too and this line would fail
    // to *compile*, not merely fail a runtime assertion.
    static_assertions::assert_not_impl_any!(FileWal: Sync);

    #[test]
    fn file_wal_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<FileWal>();
    }
}
