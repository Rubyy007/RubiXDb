//! Manifest recovery: sequential, bounded-memory frame walk with the
//! identical torn-vs-corrupt classification the WAL uses for its own
//! segments (`RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §4). Never reads
//! the whole file into one buffer — one frame (at most a few dozen
//! bytes) is materialized at a time, via a `BufReader` over the file.

use crate::persist::PFile as File;
use std::io::{self, BufReader, Read};

use crate::error::{EngineError, Result};
use crate::manifest::format::{self, ManifestEdit, FRAME_HEADER_LEN};
use crate::manifest::state::ManifestState;

/// No real edit body ever exceeds 33 bytes (`ADD_SSTABLE`, the largest
/// of the three) — generous headroom for a hypothetical future minor
/// edit-type addition, while still rejecting an implausible length
/// (corruption of the length field itself) before it can ever size an
/// allocation, per operating brief "validate before allocation."
const MAX_EDIT_BODY_LEN: usize = 64;

#[derive(Debug, Default)]
pub struct ManifestReplayResult {
    pub state: ManifestState,
    /// Byte offset immediately past the last valid frame — where the
    /// file should be considered to "end" for any future append. Equal
    /// to the file's own length unless a torn tail was found.
    pub valid_length: u64,
    pub truncated: bool,
    /// Total number of valid edits replayed — inspection/observability
    /// only (`PHASE5_ARCHITECTURE.md` §7's Manifest inspection tool).
    pub edit_count: u64,
    /// The last successfully-decoded edit, if any — inspection only.
    pub last_edit: Option<ManifestEdit>,
}

fn corrupt(detail: impl Into<String>) -> EngineError {
    EngineError::Corruption {
        detail: detail.into(),
    }
}

enum ReadOutcome {
    Full,
    /// Fewer than the requested bytes were available — a torn tail.
    Short,
}

fn try_read_exact(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<ReadOutcome> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => return Ok(ReadOutcome::Short),
            n => filled += n,
        }
    }
    Ok(ReadOutcome::Full)
}

/// Reads exactly `remaining` more bytes and returns whether every one is zero.
fn rest_is_zero(reader: &mut impl Read, remaining: u64) -> io::Result<bool> {
    let mut buf = [0u8; 4096];
    let mut left = remaining;
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        match try_read_exact(reader, &mut buf[..want])? {
            ReadOutcome::Short => return Ok(false),
            ReadOutcome::Full => {}
        }
        if buf[..want].iter().any(|b| *b != 0) {
            return Ok(false);
        }
        left -= want as u64;
    }
    Ok(true)
}

/// The smallest sector size the model and real devices use; a lost larger sector satisfies the first-sector test a fortiori.
const LOST_SECTOR_GRANULE: u64 = 512;

/// ADR-TORN-TAIL-LOST-SECTOR-01. `pos` is a frame boundary at which a frame failed validation as something other than the
/// provable tail. True iff the tail is the remainder of ONE frame whose first sector was lost: (1) the bytes up to the next
/// sector boundary are all zero, (2) the tail is no longer than one maximum-size frame, (3) no complete valid frame starts
/// anywhere behind the hole. Anything else keeps the old result (an error), so a hole in front of readable edits is never
/// truncated away.
fn lost_first_sector_tail<R: Read + std::io::Seek>(
    reader: &mut R,
    pos: u64,
    file_len: u64,
) -> io::Result<bool> {
    let tail_len = file_len.saturating_sub(pos);
    if tail_len > (FRAME_HEADER_LEN + MAX_EDIT_BODY_LEN) as u64 {
        return Ok(false);
    }
    // The first sector is the bytes from `pos` to the next sector boundary (a header that straddles the boundary is
    // therefore only partly zero).
    let zero_end = (pos / LOST_SECTOR_GRANULE + 1) * LOST_SECTOR_GRANULE;
    if zero_end >= file_len {
        return Ok(false);
    }
    reader.seek(std::io::SeekFrom::Start(pos))?;
    let mut tail = vec![0u8; tail_len as usize]; // at most 72 bytes, checked above
    reader.read_exact(&mut tail)?;
    let hole = (zero_end - pos) as usize;
    if tail[..hole].iter().any(|b| *b != 0) {
        return Ok(false);
    }
    let mut p = hole;
    while p + FRAME_HEADER_LEN < tail.len() {
        let length = u32::from_le_bytes([tail[p], tail[p + 1], tail[p + 2], tail[p + 3]]) as usize;
        if length > 0 && length <= MAX_EDIT_BODY_LEN && p + FRAME_HEADER_LEN + length <= tail.len()
        {
            let stored = u32::from_le_bytes([tail[p + 4], tail[p + 5], tail[p + 6], tail[p + 7]]);
            let body = &tail[p + FRAME_HEADER_LEN..p + FRAME_HEADER_LEN + length];
            if crc32c::crc32c(body) == stored && format::decode_body(body).is_ok() {
                return Ok(false); // a readable edit behind the hole: mid-file damage
            }
        }
        p += 1;
    }
    Ok(true)
}

/// Walks every frame in `file` from byte 0, applying valid edits to a
/// fresh `ManifestState` as they're found. `file`'s own cursor is left
/// undefined on return (callers needing to append afterward should
/// `seek` explicitly first) — this function is read-only with respect
/// to file *content* regardless of cursor position.
pub fn replay(file: &mut File) -> Result<ManifestReplayResult> {
    use std::io::{Seek, SeekFrom};

    let file_len = file.metadata()?.len();
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(&mut *file);

    let mut state = ManifestState::new();
    let mut pos: u64 = 0;
    let mut truncated = false;
    let mut edit_count: u64 = 0;
    let mut last_edit: Option<ManifestEdit> = None;

    loop {
        if pos >= file_len {
            break;
        }
        let mut header = [0u8; FRAME_HEADER_LEN];
        match try_read_exact(&mut reader, &mut header)? {
            ReadOutcome::Short => {
                truncated = true;
                break;
            }
            ReadOutcome::Full => {}
        }
        // A tail that is zero-filled to the end of the file (a file whose length was extended but whose data never reached the
        // disk) is a torn tail whatever its length: no real frame is all zero (its body holds at least an edit type byte).
        if header == [0u8; FRAME_HEADER_LEN]
            && rest_is_zero(&mut reader, file_len - pos - FRAME_HEADER_LEN as u64)?
        {
            truncated = true;
            break;
        }
        let length = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
        let stored_crc = u32::from_le_bytes(header[4..8].try_into().unwrap());

        if length > MAX_EDIT_BODY_LEN {
            if lost_first_sector_tail(&mut reader, pos, file_len)? {
                truncated = true;
                break;
            }
            return Err(corrupt(format!(
                "manifest: frame at offset {pos} declares an implausible body length {length} \
                 (max {MAX_EDIT_BODY_LEN})"
            )));
        }

        let mut body = vec![0u8; length];
        match try_read_exact(&mut reader, &mut body)? {
            ReadOutcome::Short => {
                truncated = true;
                break;
            }
            ReadOutcome::Full => {}
        }

        let actual_crc = crc32c::crc32c(&body);
        let frame_end = pos + FRAME_HEADER_LEN as u64 + length as u64;
        if actual_crc != stored_crc {
            if frame_end == file_len {
                // Trailing frame, checksum mismatch: a torn fsync
                // boundary, not corruption (RUBIC_MANIFEST_FORMAT_
                // SPECIFICATION.md §4 step 3).
                truncated = true;
                break;
            }
            if lost_first_sector_tail(&mut reader, pos, file_len)? {
                truncated = true;
                break;
            }
            return Err(corrupt(format!(
                "manifest: checksum mismatch at offset {pos} (not the trailing frame -- \
                 real corruption, not a torn write)"
            )));
        }

        let edit = match format::decode_body(&body) {
            Ok(edit) => edit,
            Err(e) => {
                if lost_first_sector_tail(&mut reader, pos, file_len)? {
                    truncated = true;
                    break;
                }
                return Err(match e {
                    EngineError::Corruption { detail } => {
                        corrupt(format!("manifest: at offset {pos}: {detail}"))
                    }
                    other => other,
                });
            }
        };
        state.apply(edit).map_err(|e| match e {
            EngineError::Corruption { detail } => {
                corrupt(format!("manifest: at offset {pos}: {detail}"))
            }
            other => other,
        })?;
        edit_count += 1;
        last_edit = Some(edit);

        pos = frame_end;
    }

    Ok(ManifestReplayResult {
        state,
        valid_length: pos,
        truncated,
        edit_count,
        last_edit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    fn temp_file() -> File {
        let path = std::env::temp_dir().join(format!(
            "rubixdb_manifest_recovery_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::persist::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap()
    }

    #[test]
    fn empty_file_replays_to_empty_state() {
        let mut f = temp_file();
        let result = replay(&mut f).unwrap();
        assert_eq!(result.valid_length, 0);
        assert!(!result.truncated);
        assert_eq!(result.state.checkpoint_seq(), 0);
        assert!(result.state.live_sstables.is_empty());
    }

    #[test]
    fn clean_multi_edit_file_replays_fully() {
        let mut f = temp_file();
        f.write_all(&format::encode_frame(&ManifestEdit::AddSstable {
            id: 1,
            min_seq: 1,
            max_seq: 10,
            file_size: 100,
        }))
        .unwrap();
        f.write_all(&format::encode_frame(&ManifestEdit::SetCheckpoint {
            flushed_through_seq: 10,
            wal_segment_id: 1,
            wal_offset: 0,
        }))
        .unwrap();
        f.flush().unwrap();

        let result = replay(&mut f).unwrap();
        assert!(!result.truncated);
        assert_eq!(result.state.live_sstables.len(), 1);
        assert_eq!(result.state.checkpoint_seq(), 10);
        assert_eq!(result.valid_length, f.metadata().unwrap().len());
    }

    #[test]
    fn torn_tail_mid_header_is_truncated_not_corrupt() {
        let mut f = temp_file();
        f.write_all(&format::encode_frame(&ManifestEdit::AddSstable {
            id: 1,
            min_seq: 1,
            max_seq: 10,
            file_size: 100,
        }))
        .unwrap();
        let valid_len = f.stream_position().unwrap();
        f.write_all(&[1, 2, 3]).unwrap(); // partial next frame header
        f.flush().unwrap();

        let result = replay(&mut f).unwrap();
        assert!(result.truncated);
        assert_eq!(result.valid_length, valid_len);
        assert_eq!(result.state.live_sstables.len(), 1);
    }

    #[test]
    fn torn_tail_mid_body_is_truncated_not_corrupt() {
        let mut f = temp_file();
        let frame = format::encode_frame(&ManifestEdit::AddSstable {
            id: 1,
            min_seq: 1,
            max_seq: 10,
            file_size: 100,
        });
        f.write_all(&frame[..frame.len() - 5]).unwrap(); // truncate mid-body
        f.flush().unwrap();

        let result = replay(&mut f).unwrap();
        assert!(result.truncated);
        assert_eq!(result.valid_length, 0);
        assert!(result.state.live_sstables.is_empty());
    }

    #[test]
    fn trailing_checksum_mismatch_is_torn_not_corrupt() {
        let mut f = temp_file();
        let mut frame = format::encode_frame(&ManifestEdit::AddSstable {
            id: 1,
            min_seq: 1,
            max_seq: 10,
            file_size: 100,
        });
        let last = frame.len() - 1;
        frame[last] ^= 0xFF; // corrupt the last body byte -> bad checksum, but this IS the tail
        f.write_all(&frame).unwrap();
        f.flush().unwrap();

        let result = replay(&mut f).unwrap();
        assert!(result.truncated);
        assert_eq!(result.valid_length, 0);
    }

    #[test]
    fn non_tail_checksum_mismatch_is_corruption() {
        let mut f = temp_file();
        let mut frame1 = format::encode_frame(&ManifestEdit::AddSstable {
            id: 1,
            min_seq: 1,
            max_seq: 10,
            file_size: 100,
        });
        let last = frame1.len() - 1;
        frame1[last] ^= 0xFF; // corrupt frame1, but frame2 follows -> not a torn tail
        f.write_all(&frame1).unwrap();
        f.write_all(&format::encode_frame(&ManifestEdit::RemoveSstable {
            id: 1,
        }))
        .unwrap();
        f.flush().unwrap();

        let err = replay(&mut f).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn implausible_length_is_corruption() {
        let mut f = temp_file();
        f.write_all(&(u32::MAX).to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.flush().unwrap();

        let err = replay(&mut f).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn checkpoint_regression_in_file_is_corruption() {
        let mut f = temp_file();
        f.write_all(&format::encode_frame(&ManifestEdit::SetCheckpoint {
            flushed_through_seq: 100,
            wal_segment_id: 1,
            wal_offset: 0,
        }))
        .unwrap();
        f.write_all(&format::encode_frame(&ManifestEdit::SetCheckpoint {
            flushed_through_seq: 50,
            wal_segment_id: 1,
            wal_offset: 0,
        }))
        .unwrap();
        f.flush().unwrap();

        let err = replay(&mut f).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn remove_never_added_in_file_is_corruption() {
        let mut f = temp_file();
        f.write_all(&format::encode_frame(&ManifestEdit::RemoveSstable {
            id: 5,
        }))
        .unwrap();
        f.flush().unwrap();

        let err = replay(&mut f).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn cursor_position_does_not_matter_on_entry() {
        let mut f = temp_file();
        f.write_all(&format::encode_frame(&ManifestEdit::AddSstable {
            id: 1,
            min_seq: 1,
            max_seq: 10,
            file_size: 100,
        }))
        .unwrap();
        f.seek(SeekFrom::Start(3)).unwrap(); // arbitrary cursor before calling replay
        let result = replay(&mut f).unwrap();
        assert_eq!(result.state.live_sstables.len(), 1);
    }
}
