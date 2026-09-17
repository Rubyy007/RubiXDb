//! Byte-exact Manifest frame/edit encode-decode, per
//! `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §2-§3. Independent of
//! `wal::format::encode_frame` (which cannot be reused directly — see
//! that function's own doc comment and the format spec's §2 note) but
//! byte-compatible with it at the frame-header level (`length:u32 LE,
//! crc32c:u32 LE, body`), verified by a cross-check test below.

use crate::error::{EngineError, Result};

pub const FRAME_HEADER_LEN: usize = 8;

pub const EDIT_ADD_SSTABLE: u8 = 1;
pub const EDIT_REMOVE_SSTABLE: u8 = 2;
pub const EDIT_SET_CHECKPOINT: u8 = 3;

/// One decoded Manifest edit — `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md`
/// §3, exactly the three edit types the authoritative spec defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestEdit {
    AddSstable {
        id: u64,
        min_seq: u64,
        max_seq: u64,
        file_size: u64,
    },
    RemoveSstable {
        id: u64,
    },
    SetCheckpoint {
        flushed_through_seq: u64,
        wal_segment_id: u64,
        wal_offset: u64,
    },
}

fn corrupt(detail: impl Into<String>) -> EngineError {
    EngineError::Corruption {
        detail: detail.into(),
    }
}

/// Encodes one edit's `body` (`edit_type(1) || type_fields`, §3.1) —
/// does not include the frame header; see `encode_frame`.
fn encode_body(edit: &ManifestEdit) -> Vec<u8> {
    match edit {
        ManifestEdit::AddSstable {
            id,
            min_seq,
            max_seq,
            file_size,
        } => {
            let mut body = Vec::with_capacity(33);
            body.push(EDIT_ADD_SSTABLE);
            body.extend_from_slice(&id.to_le_bytes());
            body.extend_from_slice(&min_seq.to_le_bytes());
            body.extend_from_slice(&max_seq.to_le_bytes());
            body.extend_from_slice(&file_size.to_le_bytes());
            body
        }
        ManifestEdit::RemoveSstable { id } => {
            let mut body = Vec::with_capacity(9);
            body.push(EDIT_REMOVE_SSTABLE);
            body.extend_from_slice(&id.to_le_bytes());
            body
        }
        ManifestEdit::SetCheckpoint {
            flushed_through_seq,
            wal_segment_id,
            wal_offset,
        } => {
            let mut body = Vec::with_capacity(25);
            body.push(EDIT_SET_CHECKPOINT);
            body.extend_from_slice(&flushed_through_seq.to_le_bytes());
            body.extend_from_slice(&wal_segment_id.to_le_bytes());
            body.extend_from_slice(&wal_offset.to_le_bytes());
            body
        }
    }
}

/// Encodes one complete frame (`length || crc32c || body`,
/// `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §2) ready to append to the
/// Manifest file.
pub fn encode_frame(edit: &ManifestEdit) -> Vec<u8> {
    let body = encode_body(edit);
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    let crc = crc32c::crc32c(&body);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame.extend_from_slice(&body);
    frame
}

fn read_u64(buf: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap())
}

/// Decodes and validates one edit's `body` bytes (already length- and
/// checksum-verified by the caller) — `RUBIC_MANIFEST_FORMAT_
/// SPECIFICATION.md` §3.2's structural checks (unknown `edit_type`,
/// wrong body length for that type, `min_seq > max_seq`). Semantic
/// checks that need cross-edit state (duplicate/invalid SSTable
/// references, checkpoint regression, §3.2's other two rules) are the
/// caller's (`ManifestState::apply`'s) responsibility, not this
/// function's — this function only ever looks at one edit in isolation.
pub fn decode_body(body: &[u8]) -> Result<ManifestEdit> {
    if body.is_empty() {
        return Err(corrupt("manifest edit: empty body (missing edit_type)"));
    }
    match body[0] {
        EDIT_ADD_SSTABLE => {
            if body.len() != 33 {
                return Err(corrupt(format!(
                    "manifest edit: ADD_SSTABLE body must be 33 bytes, got {}",
                    body.len()
                )));
            }
            let id = read_u64(body, 1);
            let min_seq = read_u64(body, 9);
            let max_seq = read_u64(body, 17);
            let file_size = read_u64(body, 25);
            if min_seq > max_seq {
                return Err(corrupt(format!(
                    "manifest edit: ADD_SSTABLE id={id} has min_seq {min_seq} > max_seq {max_seq}"
                )));
            }
            Ok(ManifestEdit::AddSstable {
                id,
                min_seq,
                max_seq,
                file_size,
            })
        }
        EDIT_REMOVE_SSTABLE => {
            if body.len() != 9 {
                return Err(corrupt(format!(
                    "manifest edit: REMOVE_SSTABLE body must be 9 bytes, got {}",
                    body.len()
                )));
            }
            Ok(ManifestEdit::RemoveSstable {
                id: read_u64(body, 1),
            })
        }
        EDIT_SET_CHECKPOINT => {
            if body.len() != 25 {
                return Err(corrupt(format!(
                    "manifest edit: SET_CHECKPOINT body must be 25 bytes, got {}",
                    body.len()
                )));
            }
            Ok(ManifestEdit::SetCheckpoint {
                flushed_through_seq: read_u64(body, 1),
                wal_segment_id: read_u64(body, 9),
                wal_offset: read_u64(body, 17),
            })
        }
        other => Err(corrupt(format!(
            "manifest edit: unknown edit_type byte {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_sstable_round_trip() {
        let edit = ManifestEdit::AddSstable {
            id: 7,
            min_seq: 10,
            max_seq: 20,
            file_size: 4096,
        };
        let frame = encode_frame(&edit);
        let length = u32::from_le_bytes(frame[0..4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(frame[4..8].try_into().unwrap());
        let body = &frame[8..8 + length];
        assert_eq!(crc, crc32c::crc32c(body));
        let decoded = decode_body(body).unwrap();
        assert_eq!(decoded, edit);
    }

    #[test]
    fn remove_sstable_round_trip() {
        let edit = ManifestEdit::RemoveSstable { id: 42 };
        let frame = encode_frame(&edit);
        let length = u32::from_le_bytes(frame[0..4].try_into().unwrap()) as usize;
        let body = &frame[8..8 + length];
        assert_eq!(decode_body(body).unwrap(), edit);
    }

    #[test]
    fn set_checkpoint_round_trip() {
        let edit = ManifestEdit::SetCheckpoint {
            flushed_through_seq: 100,
            wal_segment_id: 3,
            wal_offset: 512,
        };
        let frame = encode_frame(&edit);
        let length = u32::from_le_bytes(frame[0..4].try_into().unwrap()) as usize;
        let body = &frame[8..8 + length];
        assert_eq!(decode_body(body).unwrap(), edit);
    }

    #[test]
    fn rejects_unknown_edit_type() {
        let body = vec![99u8, 0, 0, 0, 0, 0, 0, 0, 0];
        let err = decode_body(&body).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn rejects_wrong_length_for_add_sstable() {
        let body = vec![EDIT_ADD_SSTABLE, 0, 0, 0];
        let err = decode_body(&body).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn rejects_min_seq_greater_than_max_seq() {
        let edit = ManifestEdit::AddSstable {
            id: 1,
            min_seq: 100,
            max_seq: 50,
            file_size: 10,
        };
        let body = encode_body(&edit);
        let err = decode_body(&body).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn rejects_empty_body() {
        let err = decode_body(&[]).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    /// Byte-compatibility cross-check against `wal::format`'s own frame
    /// header shape (`length:u32 LE, crc32c:u32 LE`), per `RUBIC_
    /// MANIFEST_FORMAT_SPECIFICATION.md` §2 -- same header layout,
    /// independently implemented.
    #[test]
    fn frame_header_matches_wal_frame_header_shape() {
        let edit = ManifestEdit::RemoveSstable { id: 1 };
        let frame = encode_frame(&edit);
        assert_eq!(frame.len(), FRAME_HEADER_LEN + 9);
        let declared_len = u32::from_le_bytes(frame[0..4].try_into().unwrap()) as usize;
        assert_eq!(declared_len, 9);
        let crc = u32::from_le_bytes(frame[4..8].try_into().unwrap());
        assert_eq!(crc, crc32c::crc32c(&frame[8..]));
    }
}
