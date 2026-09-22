//! Pure byte-level encode/decode for the WAL's segment header and record
//! frame, per WAL Spec §2. No I/O happens in this module — it only turns
//! in-memory values into bytes and back, so it is trivially unit-testable
//! and reusable by the Manifest (LSM Engine Spec §6.1, which "reuses the
//! WAL's exact frame format").
//!
//! Every function that reads a length-prefixed field from a byte slice
//! validates that length against the slice's actual remaining bytes (or
//! against `MAX_RECORD_LEN`) before it is used to size a `Vec` or index
//! further into the buffer — this is the WAL Spec §2.6 pattern, applied
//! everywhere a length-prefixed field appears (Architecture Spec §12).
//! `read_u32_le`/`read_u64_le` return `Result` rather than relying on a
//! `debug_assert!`-only precondition for exactly this reason: a
//! `debug_assert!` disappears in release builds, and untrusted on-disk
//! bytes must never be able to reach an indexing panic in *any* build.

use crate::error::{EngineError, Result};

/// `"RBXWALv1"` — WAL Spec §2.2.
pub const SEGMENT_MAGIC: [u8; 8] = *b"RBXWALv1";
/// Fixed size of the segment header — WAL Spec §2.2.
pub const SEGMENT_HEADER_LEN: usize = 24;
/// WAL Spec §2.2's `format_version` for this specification.
pub const FORMAT_VERSION: u32 = 1;
/// `length:u32 LE, crc32c:u32 LE` — WAL Spec §2.3.
pub const FRAME_HEADER_LEN: usize = 8;

/// WAL Spec §2.4 `op` byte values. Identical to LSM Engine Spec §0.2's
/// reused values for `PUT`/`DELETE`.
pub const OP_PUT: u8 = 1;
pub const OP_DELETE: u8 = 2;
pub const OP_CHECKPOINT_MARKER: u8 = 3;
/// Reserved by WAL Spec §2.4; not emitted before Phase 5. Recognized here
/// only so recovery can name it explicitly in an error rather than falling
/// through to "unknown op byte".
pub const OP_ENGINE_SWITCH: u8 = 4;
/// `RELATIONAL ADR AMENDMENT 001` AA.2: one frame atomically encoding N
/// `PUT`/`DELETE` operations under one shared `seq` — the storage
/// primitive `LsmEngine::write_batch` requires. Additive: `PUT`/
/// `DELETE`/`CHECKPOINT_MARKER`'s existing byte layout and meaning are
/// unchanged, and the frame-level `length || crc32c` envelope (this
/// module) needs no changes at all to carry it — only `wal::ops`'s
/// encode/decode match arms are extended.
pub const OP_GROUP: u8 = 5;

/// WAL Spec §2.6 default. Also used, independently, as the SSTable record
/// format's length-field bound (LSM Engine Spec §2.2) via the same
/// `checked_u32_len` pattern below.
pub const DEFAULT_MAX_RECORD_LEN: usize = 64 * 1024 * 1024;
/// WAL Spec §3.4/§4/§7 default.
pub const DEFAULT_MAX_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;

/// Decoded segment header fields — WAL Spec §2.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    pub format_version: u32,
    pub segment_id: u64,
    pub flags: u32,
}

/// Builds a segment header's 24 on-disk bytes. `flags` is always 0 (WAL
/// Spec §2.2: "Reserved, must be 0 in v1").
pub fn encode_segment_header(segment_id: u64) -> [u8; SEGMENT_HEADER_LEN] {
    let mut buf = [0u8; SEGMENT_HEADER_LEN];
    buf[0..8].copy_from_slice(&SEGMENT_MAGIC);
    buf[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf[12..20].copy_from_slice(&segment_id.to_le_bytes());
    buf[20..24].copy_from_slice(&0u32.to_le_bytes());
    buf
}

/// Decodes and validates a segment header. `expected_segment_id` is the ID
/// parsed from the segment's filename (WAL Spec §2.1); a mismatch against
/// the header's own `segment_id` field is itself a validation failure
/// (WAL Spec §3.2: "cross-checked on open"). Also rejects any
/// `format_version` other than the one this build understands, and any
/// non-zero reserved `flags` — WAL Spec §2.2 pins `flags` at "must be 0 in
/// v1", and silently accepting a future/foreign format here would mean
/// misinterpreting its frames as if they were this version's, which is a
/// worse failure mode than refusing to read it at all.
///
/// A short or malformed header is never a torn write (WAL Spec §6.2 step 1:
/// "a torn write only ever damages the frame currently being appended,
/// never the segment header itself, which is written once and fsynced
/// before any records") — so this always returns `Corruption`, never a
/// truncation signal.
pub fn decode_segment_header(buf: &[u8], expected_segment_id: u64) -> Result<SegmentHeader> {
    if buf.len() < SEGMENT_HEADER_LEN {
        return Err(EngineError::Corruption {
            detail: format!(
                "segment header truncated: {} of {SEGMENT_HEADER_LEN} bytes present",
                buf.len()
            ),
        });
    }
    if buf[0..8] != SEGMENT_MAGIC {
        return Err(EngineError::Corruption {
            detail: "segment header magic mismatch".to_string(),
        });
    }
    let format_version = read_u32_le(&buf[8..12])?;
    let segment_id = read_u64_le(&buf[12..20])?;
    let flags = read_u32_le(&buf[20..24])?;
    if format_version != FORMAT_VERSION {
        return Err(EngineError::Corruption {
            detail: format!("unsupported format_version {format_version}"),
        });
    }
    if flags != 0 {
        return Err(EngineError::Corruption {
            detail: "non-zero reserved flags".to_string(),
        });
    }
    if segment_id != expected_segment_id {
        return Err(EngineError::Corruption {
            detail: format!(
                "segment header id {segment_id} does not match filename-derived id {expected_segment_id}"
            ),
        });
    }
    Ok(SegmentHeader {
        format_version,
        segment_id,
        flags,
    })
}

/// Encodes one full WAL frame (`length || crc32c || body`, WAL Spec §2.3),
/// where `body := seq(8) || op_tag(1) || op_body`. `encode_op_body` writes
/// the `op` tag byte and its `op_body` into `out` and returns the tag byte
/// — fallible (`write_len_prefixed`'s own per-field bound check can fail
/// and propagate here, rather than needing a separate after-the-fact
/// total-length check to catch what a per-field check already caught).
///
/// **Not directly reusable by the Manifest** (correcting an earlier,
/// inaccurate version of this comment): this function always prepends an
/// 8-byte `seq` before the op tag, per the WAL's own record-body layout —
/// but the Manifest's body layout (LSM Engine Spec §6.1) is `edit_type(1)
/// || type_fields`, with no `seq` prefix at all. Phase 5's
/// `src/manifest/format.rs` implements its own minimal `length || crc32c
/// || body` wrapper (byte-compatible with this one at the frame-header
/// level, cross-checked by a dedicated test) rather than this function
/// being refactored to drop the baked-in `seq` field — refactoring this
/// already-certified, 200+-test-covered WAL primitive for that purpose
/// was judged higher-risk than a ~15-line independent implementation
/// (`PHASE5_ADR.md`).
///
/// Returns `CapacityExceeded` (before writing anything) if the encoded
/// body would exceed `max_record_len` or `u32::MAX` (the `length` field's
/// own on-disk width, WAL Spec §2.3) — checked before any allocation sized
/// by an unvalidated value, per the WAL Spec §2.6 pattern.
pub fn encode_frame(
    seq: u64,
    max_record_len: usize,
    encode_op_body: impl FnOnce(&mut Vec<u8>) -> Result<u8>,
) -> Result<Vec<u8>> {
    let mut op_body = Vec::new();
    let op_tag = encode_op_body(&mut op_body)?;

    let body_len = 8usize
        .checked_add(1)
        .and_then(|n| n.checked_add(op_body.len()))
        .ok_or(EngineError::CapacityExceeded {
            requested: u64::MAX,
            max: max_record_len as u64,
        })?;

    let effective_max = max_record_len.min(u32::MAX as usize);
    if body_len > effective_max {
        return Err(EngineError::CapacityExceeded {
            requested: body_len as u64,
            max: effective_max as u64,
        });
    }

    let mut body = Vec::with_capacity(body_len);
    body.extend_from_slice(&seq.to_le_bytes());
    body.push(op_tag);
    body.extend_from_slice(&op_body);
    debug_assert_eq!(body.len(), body_len);

    let crc = crc32c::crc32c(&body);
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + body.len());
    frame.extend_from_slice(&(body_len as u32).to_le_bytes());
    frame.extend_from_slice(&crc.to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Converts a `usize` length into the on-disk `u32 LE` width used by every
/// length-prefixed field in this format (WAL Spec §2.4's `key_len`/
/// `val_len`), rejecting rather than silently truncating a value that
/// doesn't fit — the WAL Spec §2.6 pattern applied to nested fields.
pub fn checked_u32_len(n: usize, max_record_len: usize) -> Result<u32> {
    u32::try_from(n).map_err(|_| EngineError::CapacityExceeded {
        requested: n as u64,
        max: max_record_len as u64,
    })
}

/// Appends a `len:u32 LE, bytes` pair to `out`, per the WAL Spec §2.4
/// `key_len`/`val_len` convention. Fallible (Group 2.3): unlike an
/// earlier version of this helper, an oversized field is rejected here,
/// at the point of the actual violation, rather than silently emitting a
/// sentinel value and relying on a *different*, later check elsewhere to
/// catch it — one guard, one place, no room for the two to drift apart.
pub fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8], max_record_len: usize) -> Result<()> {
    let len = checked_u32_len(bytes.len(), max_record_len)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// Reads a `len:u32 LE, bytes` pair from the front of `buf`, validating
/// `len` against `buf`'s actual remaining length *before* slicing — never
/// trusts a corrupted length field to index past what's actually present.
/// Returns the field's bytes and the remainder of `buf` after it.
pub fn read_len_prefixed(buf: &[u8]) -> Result<(&[u8], &[u8])> {
    if buf.len() < 4 {
        return Err(EngineError::Corruption {
            detail: "truncated length-prefixed field: fewer than 4 bytes remain".to_string(),
        });
    }
    let len = read_u32_le(&buf[0..4])? as usize;
    let rest = &buf[4..];
    if len > rest.len() {
        return Err(EngineError::Corruption {
            detail: format!(
                "length-prefixed field declares {len} bytes but only {} remain",
                rest.len()
            ),
        });
    }
    Ok((&rest[..len], &rest[len..]))
}

/// Reads a little-endian `u32` from the first 4 bytes of `b`. Returns
/// `Corruption` rather than panicking if `b` is shorter than 4 bytes —
/// Group 2.2: this used to be a panic-on-misuse precondition guarded only
/// by a `debug_assert!` (compiled out in release), which meant a caller
/// bug or an unanticipated short slice from untrusted input could panic
/// in a release build. Every call site in this crate now handles the
/// `Err` case instead of relying on the length always being right.
pub fn read_u32_le(b: &[u8]) -> Result<u32> {
    let bytes: [u8; 4] =
        b.get(0..4)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| EngineError::Corruption {
                detail: format!("expected at least 4 bytes to decode a u32, got {}", b.len()),
            })?;
    Ok(u32::from_le_bytes(bytes))
}

/// Reads a little-endian `u64` from the first 8 bytes of `b`. See
/// `read_u32_le`.
pub fn read_u64_le(b: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] =
        b.get(0..8)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| EngineError::Corruption {
                detail: format!("expected at least 8 bytes to decode a u64, got {}", b.len()),
            })?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_header_round_trip() {
        let bytes = encode_segment_header(42);
        let header = decode_segment_header(&bytes, 42).unwrap();
        assert_eq!(header.format_version, FORMAT_VERSION);
        assert_eq!(header.segment_id, 42);
        assert_eq!(header.flags, 0);
    }

    #[test]
    fn segment_header_rejects_short_buffer() {
        let bytes = encode_segment_header(1);
        let err = decode_segment_header(&bytes[..10], 1).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn segment_header_rejects_bad_magic() {
        let mut bytes = encode_segment_header(1);
        bytes[0] = b'X';
        let err = decode_segment_header(&bytes, 1).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn segment_header_rejects_id_mismatch() {
        let bytes = encode_segment_header(1);
        let err = decode_segment_header(&bytes, 2).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    /// Group 2.1 regression test.
    #[test]
    fn segment_header_rejects_unsupported_format_version() {
        let mut bytes = encode_segment_header(1);
        bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
        let err = decode_segment_header(&bytes, 1).unwrap_err();
        match err {
            EngineError::Corruption { detail } => {
                assert!(detail.contains("format_version"), "detail was: {detail}")
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    /// Group 2.1 regression test.
    #[test]
    fn segment_header_rejects_non_zero_flags() {
        let mut bytes = encode_segment_header(1);
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        let err = decode_segment_header(&bytes, 1).unwrap_err();
        match err {
            EngineError::Corruption { detail } => {
                assert!(detail.contains("flags"), "detail was: {detail}")
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[test]
    fn frame_encode_rejects_oversized_body() {
        let err = encode_frame(1, 4, |out| {
            out.extend_from_slice(&[0u8; 100]);
            Ok(OP_CHECKPOINT_MARKER)
        })
        .unwrap_err();
        assert!(matches!(err, EngineError::CapacityExceeded { .. }));
    }

    /// Group 2.3 regression test: a fallible `encode_op_body` closure's
    /// error must propagate out of `encode_frame` as-is, not be swallowed
    /// or turned into a malformed frame.
    #[test]
    fn frame_encode_propagates_op_body_error() {
        let err = encode_frame(1, DEFAULT_MAX_RECORD_LEN, |_out| {
            Err(EngineError::CapacityExceeded {
                requested: 1,
                max: 0,
            })
        })
        .unwrap_err();
        assert!(matches!(err, EngineError::CapacityExceeded { .. }));
    }

    #[test]
    fn len_prefixed_round_trip() {
        let mut buf = Vec::new();
        write_len_prefixed(&mut buf, b"hello", DEFAULT_MAX_RECORD_LEN).unwrap();
        let (field, rest) = read_len_prefixed(&buf).unwrap();
        assert_eq!(field, b"hello");
        assert!(rest.is_empty());
    }

    #[test]
    fn len_prefixed_rejects_overclaimed_length() {
        // Declares a 100-byte field but only supplies 2 bytes of payload.
        let mut buf = 100u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[1, 2]);
        let err = read_len_prefixed(&buf).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    /// Group 2.2 regression test.
    #[test]
    fn read_u32_le_rejects_short_slice() {
        let err = read_u32_le(&[1, 2]).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    /// Group 2.2 regression test.
    #[test]
    fn read_u64_le_rejects_short_slice() {
        let err = read_u64_le(&[1, 2, 3]).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn read_u32_le_round_trips() {
        let bytes = 0xDEAD_BEEFu32.to_le_bytes();
        assert_eq!(read_u32_le(&bytes).unwrap(), 0xDEAD_BEEF);
    }

    #[test]
    fn read_u64_le_round_trips() {
        let bytes = 0x1122_3344_5566_7788u64.to_le_bytes();
        assert_eq!(read_u64_le(&bytes).unwrap(), 0x1122_3344_5566_7788);
    }
}
