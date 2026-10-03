//! Crash-safe restore of a `RUBXBKUP` backup into a FRESH data directory.
//!
//! Protocol (every step is a deterministic crash point, see [`RestorePhase`]):
//!
//! 1. `verify_backup` the whole file first — nothing is written for a backup
//!    that does not verify.
//! 2. Refuse unless the destination does not exist or is an empty directory.
//!    Existing data is never replaced (there is no overwrite option).
//! 3. Build the new database in a *staging* directory next to the
//!    destination (`.<name>.restoring-<id>`) that carries a
//!    `RESTORE_IN_PROGRESS` marker. The destination name does not exist
//!    until the very last step.
//! 4. Open the engine there with production durability settings, apply every
//!    entry through atomic `write_batch` calls, then **re-scan the restored
//!    engine and compare entry count and content digest with the backup's**.
//! 5. Run the logical integrity check on the restored database; any
//!    `Error` finding fails the restore.
//! 6. Clean shutdown, then promote with ONE directory rename and remove the
//!    marker afterwards. A crash before the rename leaves the destination
//!    absent and a marked staging directory; a crash after leaves a complete,
//!    verified database (possibly still carrying a harmless marker file).
//!
//! The next restore to the same destination removes stale staging
//! directories — only ones that match the naming pattern AND contain the
//! marker.

use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::lsm::WriteOp;
use crate::ops::backup::{read_backup, verify_backup, ContentDigest, VerifyReport};
use crate::ops::check::{check_engine, CheckOptions, CheckReport};
use crate::ops::open::open_engine_for_ops;
use crate::ops::{codes, unique_id_hex, OpsError};

pub const MARKER_FILE: &str = "RESTORE_IN_PROGRESS";
const STAGING_INFIX: &str = ".restoring-";
const BATCH_MAX_OPS: usize = 500;
const BATCH_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Deterministic crash/failure points, reported to `RestoreOptions::hook`
/// *after* the named step completed. A test harness aborts the process (or
/// returns an error) from the hook to exercise every window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePhase {
    Verified,
    StagingCreated,
    EngineOpened,
    /// After the `n`th (1-based) batch was committed.
    BatchApplied(u64),
    Loaded,
    DigestVerified,
    CheckPassed,
    EngineClosed,
    /// After the rename: the database now exists at the destination.
    Promoted,
    MarkerRemoved,
}

#[derive(Default)]
pub struct RestoreOptions<'a> {
    pub hook: Option<&'a dyn Fn(RestorePhase) -> Result<(), OpsError>>,
}

#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub destination: PathBuf,
    pub backup_id: String,
    pub snapshot_seq: u64,
    pub entries: u64,
    pub content_digest: u64,
    pub verify: VerifyReport,
    pub check: CheckReport,
    pub stale_staging_removed: usize,
    pub verify_duration: Duration,
    pub load_duration: Duration,
    pub check_duration: Duration,
    pub total_duration: Duration,
}

fn at(opts: &RestoreOptions<'_>, p: RestorePhase) -> Result<(), OpsError> {
    match opts.hook {
        Some(h) => h(p),
        None => Ok(()),
    }
}

fn staging_prefix(dest_name: &str) -> String {
    format!(".{dest_name}{STAGING_INFIX}")
}

/// Removes leftover staging directories for `dest` from an earlier crashed
/// restore. Conservative: name pattern AND marker file must both match.
pub fn clean_stale_staging(dest: &Path) -> Result<usize, OpsError> {
    let (Some(parent), Some(name)) = (dest.parent(), dest.file_name().and_then(|n| n.to_str()))
    else {
        return Ok(0);
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let prefix = staging_prefix(name);
    let mut removed = 0;
    let Ok(rd) = std::fs::read_dir(parent) else {
        return Ok(0);
    };
    for e in rd.flatten() {
        let fname = e.file_name().to_string_lossy().to_string();
        if fname.starts_with(&prefix) && e.path().join(MARKER_FILE).is_file() {
            std::fs::remove_dir_all(e.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

pub fn restore_backup(
    src: &Path,
    dest: &Path,
    opts: &RestoreOptions<'_>,
) -> Result<RestoreReport, OpsError> {
    let started = Instant::now();

    // 1. Verify first.
    let t = Instant::now();
    let verify = verify_backup(src)?;
    let verify_duration = t.elapsed();
    at(opts, RestorePhase::Verified)?;

    // 2. Destination rules.
    let dest_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| OpsError::new(codes::BAD_NAME, "destination has no usable final component"))?
        .to_string();
    let mut replace_empty_dir = false;
    if dest.exists() {
        if dest.is_dir() && std::fs::read_dir(dest)?.next().is_none() {
            replace_empty_dir = true;
        } else {
            return Err(OpsError::new(
                codes::DEST_NOT_EMPTY,
                format!(
                    "{} already exists and is not an empty directory; restore never overwrites existing data",
                    dest.display()
                ),
            ));
        }
    }
    let parent = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent)?;
    let stale = clean_stale_staging(dest)?;

    // 3. Staging directory + marker.
    let staging = parent.join(format!("{}{}", staging_prefix(&dest_name), unique_id_hex()));
    std::fs::create_dir(&staging)?;
    std::fs::write(
        staging.join(MARKER_FILE),
        format!(
            "rubixdb restore in progress\nbackup_id={}\n",
            verify.header.backup_id
        ),
    )?;
    let result = restore_into_staging(src, &staging, &verify, opts, verify_duration);
    let (load_duration, check_duration, digest, check) = match result {
        Ok(v) => v,
        Err(e) => {
            // Failed restores clean up after themselves (a crash cannot; the
            // marker lets the next restore do it).
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    // 6. Promote with ONE rename. The marker stays inside the staging
    //    directory until after the rename: every directory still carrying the
    //    staging name is therefore marked and cleanable, and a database that
    //    was promoted is complete (everything was verified before the rename).
    if replace_empty_dir {
        std::fs::remove_dir(dest)?;
    }
    if let Err(e) = std::fs::rename(&staging, dest) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e.into());
    }
    at(opts, RestorePhase::Promoted)?;
    // Best effort: a leftover marker in a promoted directory is harmless.
    let _ = std::fs::remove_file(dest.join(MARKER_FILE));
    at(opts, RestorePhase::MarkerRemoved)?;

    Ok(RestoreReport {
        destination: dest.to_path_buf(),
        backup_id: verify.header.backup_id.clone(),
        snapshot_seq: verify.header.snapshot_seq,
        entries: digest.entries,
        content_digest: digest.finish(),
        verify,
        check,
        stale_staging_removed: stale,
        verify_duration,
        load_duration,
        check_duration,
        total_duration: started.elapsed(),
    })
}

fn restore_into_staging(
    src: &Path,
    staging: &Path,
    verify: &VerifyReport,
    opts: &RestoreOptions<'_>,
    _verify_duration: Duration,
) -> Result<(Duration, Duration, ContentDigest, CheckReport), OpsError> {
    at(opts, RestorePhase::StagingCreated)?;
    let engine = open_engine_for_ops(staging)?;
    at(opts, RestorePhase::EngineOpened)?;

    // 4a. Load.
    let t = Instant::now();
    let mut ops: Vec<WriteOp> = Vec::with_capacity(BATCH_MAX_OPS);
    let mut bytes = 0usize;
    let mut batches = 0u64;
    let mut apply_err: Option<OpsError> = None;
    let flush =
        |ops: &mut Vec<WriteOp>, bytes: &mut usize, batches: &mut u64| -> Result<(), OpsError> {
            if ops.is_empty() {
                return Ok(());
            }
            engine.write_batch(ops)?;
            ops.clear();
            *bytes = 0;
            *batches += 1;
            at(opts, RestorePhase::BatchApplied(*batches))
        };
    let read = read_backup(src, |k, v| {
        ops.push(WriteOp::Put {
            key: k.to_vec(),
            value: v.to_vec(),
        });
        bytes += k.len() + v.len();
        if ops.len() >= BATCH_MAX_OPS || bytes >= BATCH_MAX_BYTES {
            if let Err(e) = flush(&mut ops, &mut bytes, &mut batches) {
                apply_err = Some(e.clone());
                return Err(e);
            }
        }
        Ok(())
    });
    if let Some(e) = apply_err {
        let _ = engine.shutdown();
        return Err(e);
    }
    let (_, read_digest, _, _, _) = match read {
        Ok(v) => v,
        Err(e) => {
            let _ = engine.shutdown();
            return Err(e);
        }
    };
    if let Err(e) = flush(&mut ops, &mut bytes, &mut batches) {
        let _ = engine.shutdown();
        return Err(e);
    }
    let load_duration = t.elapsed();
    at(opts, RestorePhase::Loaded)?;

    // 4b. Independent re-scan of the restored engine vs the backup's digest.
    let mut restored = ContentDigest::default();
    for item in engine.range(Bound::Unbounded, Bound::Unbounded) {
        let (k, v) = item?;
        restored.add(&k, &v);
    }
    if restored.entries != verify.entries || restored.finish() != verify.content_digest {
        let _ = engine.shutdown();
        return Err(OpsError::new(
            codes::RESTORE_VERIFY,
            format!(
                "restored database holds {} entries (digest {:016x}); the backup holds {} (digest {:016x})",
                restored.entries,
                restored.finish(),
                verify.entries,
                verify.content_digest
            ),
        ));
    }
    at(opts, RestorePhase::DigestVerified)?;

    // 5. Logical integrity of the restored database.
    let t = Instant::now();
    let check = check_engine(&engine, &CheckOptions::default());
    let check_duration = t.elapsed();
    if !check.is_clean() {
        let _ = engine.shutdown();
        let first = check
            .findings
            .iter()
            .find(|f| f.severity == crate::ops::check::Severity::Error)
            .map(|f| format!("{} {}: {}", f.code, f.object, f.detail))
            .unwrap_or_else(|| "check did not complete".to_string());
        return Err(OpsError::new(
            codes::RESTORE_VERIFY,
            format!(
                "restored database failed the integrity check ({} error(s)); first: {first}",
                check.errors
            ),
        ));
    }
    at(opts, RestorePhase::CheckPassed)?;

    // 6a. Clean shutdown (drains and stops the engine's background threads).
    let report = engine.shutdown();
    if !report.fully_drained {
        return Err(OpsError::new(
            codes::RESTORE_VERIFY,
            "engine did not drain cleanly at the end of the restore",
        ));
    }
    drop(engine);
    at(opts, RestorePhase::EngineClosed)?;
    Ok((load_duration, check_duration, read_digest, check))
}
