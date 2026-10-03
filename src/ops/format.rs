//! Data-directory format marker: "never silently open an incompatible
//! database with an incompatible binary".
//!
//! Status of the individual on-disk formats (verified in source):
//! * WAL segments: magic `RBXWALv1` + `format_version` — a reader refuses any
//!   other version (`wal::format::decode_segment_header`).
//! * SSTables: footer magic + `format_version` — `EngineError::Unsupported`.
//! * Catalog / table rows: `ROW_FORMAT_VERSION` byte in every row envelope.
//! * Backups: preamble + header version (`ops::backup`).
//! * **MANIFEST: frames only — no file magic, no format version.** A future
//!   incompatible manifest would be rejected only as unknown edit types /
//!   corruption (fail-closed, but unversioned). Adding a header is a change to
//!   the certified manifest and is recorded as an engine ADR item, not done
//!   here.
//!
//! This file closes the directory-level gap without touching the engine: a
//! `DATA_FORMAT` marker written when a directory is **created** by a product
//! entry point and checked before the engine opens it. A directory that
//! carries a marker this build does not support is refused and left exactly
//! as it was. A pre-existing directory with no marker is a *legacy* v1
//! directory (every file inside is individually versioned and validated); it
//! is accepted and — deliberately — not modified.

use std::path::Path;

use crate::ops::{codes, OpsError};

pub const DATA_FORMAT_FILE: &str = "DATA_FORMAT";
/// The directory-level format this build writes and reads.
pub const CURRENT_DATA_FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatState {
    /// Directory absent or empty: the caller creates it and then `stamp`s it.
    Fresh,
    /// Marker present and supported.
    Marked(u32),
    /// Non-empty directory without a marker (created by an earlier release).
    Legacy,
}

pub const UNSUPPORTED_DATA_FORMAT: &str = "UNSUPPORTED_DATA_FORMAT";

fn parse_marker(text: &str) -> Option<u32> {
    let mut found = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("rubixdb-data-format=") {
            if found.is_some() {
                return None;
            }
            found = v.trim().parse::<u32>().ok();
        }
    }
    found
}

/// Read-only compatibility decision; never writes.
pub fn ensure_compatible(dir: &Path) -> Result<FormatState, OpsError> {
    if !dir.exists() {
        return Ok(FormatState::Fresh);
    }
    let marker = dir.join(DATA_FORMAT_FILE);
    match std::fs::read_to_string(&marker) {
        Ok(text) => match parse_marker(&text) {
            Some(CURRENT_DATA_FORMAT) => Ok(FormatState::Marked(CURRENT_DATA_FORMAT)),
            Some(other) => Err(OpsError::new(
                UNSUPPORTED_DATA_FORMAT,
                format!(
                    "the data directory is format {other}; this build supports format \
                     {CURRENT_DATA_FORMAT}. Open it with a build that supports format {other} or \
                     restore a backup into a new instance. The directory has not been modified."
                ),
            )),
            None => Err(OpsError::new(
                UNSUPPORTED_DATA_FORMAT,
                "the data directory's DATA_FORMAT marker is unreadable; refusing to open it \
                 (the directory has not been modified)",
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A restore's own in-progress marker does not make the staging
            // directory "non-empty": it is still a brand-new database.
            let only_restore_marker = std::fs::read_dir(dir)?
                .flatten()
                .all(|e| e.file_name() == crate::ops::restore::MARKER_FILE);
            if only_restore_marker {
                Ok(FormatState::Fresh)
            } else {
                Ok(FormatState::Legacy)
            }
        }
        Err(e) => Err(OpsError::new(codes::IO, e.to_string())),
    }
}

/// Product-layer startup guard: the directory-format decision **plus** a
/// read-only WAL integrity preflight, run before the engine opens (and
/// therefore before any recovery can modify the directory).
///
/// Why the preflight exists: `LsmEngine::open` discards the replay summary
/// (`let _summary = wal::replay_streaming(..)`, `_replay`), although the WAL
/// documents that "the caller is already required to halt on non-empty
/// corrupted_segments". A WAL segment with a bad header, an unsupported
/// version or a mid-segment bad frame is therefore silently dropped — with
/// every segment after it — and the engine opens anyway (reproduced:
/// `physical_tests::engine_open_ignores_a_corrupt_wal_segment`). Changing
/// that is an engine change (ENGINE BOUNDARY: see
/// `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md`, "WAL corruption does not halt
/// engine open"); this guard makes every *product* entry point fail closed
/// without touching the engine.
pub fn startup_guard(dir: &Path) -> Result<FormatState, OpsError> {
    let state = ensure_compatible(dir)?;
    if dir.join("wal").is_dir() {
        let cfg = crate::wal::WalConfig::default();
        match crate::wal::replay_streaming(dir, &cfg, |_, _| Ok(())) {
            Ok(s) if s.corrupted_segments_count == 0 => {}
            Ok(s) => {
                return Err(OpsError::new(
                    codes::WAL_CORRUPT,
                    format!(
                        "{} corrupted WAL segment(s) found; refusing to open: the engine would                          silently discard them and everything after them. Run `rubixdb check`                          and restore a verified backup into a new instance. The directory has                          not been modified.",
                        s.corrupted_segments_count
                    ),
                ))
            }
            Err(e) => {
                return Err(OpsError::new(
                    codes::WAL_CORRUPT,
                    format!("the WAL cannot be read ({e}); refusing to open. The directory has not been modified."),
                ))
            }
        }
    }
    Ok(state)
}

/// Writes the marker (atomically) iff the directory is `Fresh`/empty or
/// already marked; never touches a legacy directory.
pub fn stamp_if_fresh(dir: &Path, state: FormatState) -> Result<(), OpsError> {
    if state != FormatState::Fresh {
        return Ok(());
    }
    let marker = dir.join(DATA_FORMAT_FILE);
    if marker.exists() {
        return Ok(());
    }
    let tmp = dir.join(format!("{DATA_FORMAT_FILE}.tmp"));
    std::fs::write(
        &tmp,
        format!(
            "rubixdb-data-format={CURRENT_DATA_FORMAT}\nwal-segment=1\nsstable=1\ncatalog-row=1\n"
        ),
    )?;
    std::fs::rename(&tmp, &marker)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("rubixdb_fmt_{tag}_{}", crate::ops::unique_id_hex()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn fresh_then_stamped_then_marked() {
        let d = tmp("fresh");
        assert_eq!(ensure_compatible(&d).unwrap(), FormatState::Fresh);
        stamp_if_fresh(&d, FormatState::Fresh).unwrap();
        assert_eq!(
            ensure_compatible(&d).unwrap(),
            FormatState::Marked(CURRENT_DATA_FORMAT)
        );
        assert_eq!(
            ensure_compatible(&d.join("does-not-exist")).unwrap(),
            FormatState::Fresh
        );
    }

    #[test]
    fn legacy_directory_is_accepted_and_never_modified() {
        let d = tmp("legacy");
        std::fs::write(d.join("MANIFEST"), b"x").unwrap();
        let st = ensure_compatible(&d).unwrap();
        assert_eq!(st, FormatState::Legacy);
        stamp_if_fresh(&d, st).unwrap();
        assert!(!d.join(DATA_FORMAT_FILE).exists());
    }

    #[test]
    fn an_unsupported_or_garbled_marker_is_refused_without_writing() {
        for content in [
            "rubixdb-data-format=2\n",
            "rubixdb-data-format=abc\n",
            "",
            "x=1\n",
            "rubixdb-data-format=1\nrubixdb-data-format=1\n",
        ] {
            let d = tmp("bad");
            std::fs::write(d.join(DATA_FORMAT_FILE), content).unwrap();
            std::fs::write(d.join("MANIFEST"), b"x").unwrap();
            let before: Vec<_> = std::fs::read_dir(&d)
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            let err = ensure_compatible(&d).unwrap_err();
            assert_eq!(err.code, UNSUPPORTED_DATA_FORMAT, "{content:?}");
            let after: Vec<_> = std::fs::read_dir(&d)
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert_eq!(before.len(), after.len());
            assert_eq!(
                std::fs::read_to_string(d.join(DATA_FORMAT_FILE)).unwrap(),
                content
            );
        }
    }
}
