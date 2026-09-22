//! The frame-walking core of WAL Spec §6: sequentially reads frames from a
//! single segment and classifies the end of the walk as either a clean
//! finish, a torn tail (§6.3: expected, truncate and succeed), or
//! corruption (§6.3: escalate, never truncate).
//!
//! Generic over `R: Read + Seek` rather than `WalFile` specifically, so
//! this same function is exercised directly against in-memory buffers
//! (`std::io::Cursor`, `wal::file_io::MemFile`) in unit tests and the
//! proptest fuzz harness, and against real segment files in integration
//! tests and production — the classification logic itself never touches a
//! filesystem API.

use std::io::{self, Read, Seek, SeekFrom};

use crate::wal::format::{
    decode_segment_header, read_u32_le, FRAME_HEADER_LEN, SEGMENT_HEADER_LEN,
};
use crate::wal::ops::{decode_wal_body, WalOpOwned};

/// The result of walking one already-header-validated segment from just
/// after its header to either clean EOF, a torn tail, or a corruption
/// point.
#[derive(Debug, Default)]
pub struct WalkOutcome {
    /// Every fully valid record found, in file order.
    pub records: Vec<(u64, WalOpOwned)>,
    /// Byte offset immediately after the last fully valid record — i.e.,
    /// where a torn tail begins, or the clean end of segment if none.
    /// Meaningless (equal to the corruption point) when `corrupted` is
    /// true, since nothing past a corruption point is trusted.
    pub valid_end_offset: u64,
    /// A torn write was found at the very end of the scanned bytes (WAL
    /// Spec §6.3: expected, not alarming).
    pub truncated: bool,
    /// A frame failed validation somewhere that is *not* provably the tail
    /// (WAL Spec §6.3: real corruption, must be escalated).
    pub corrupted: bool,
    pub corruption_offset: Option<u64>,
    pub corruption_detail: Option<String>,
}

/// Walks frames starting at `SEGMENT_HEADER_LEN` up to `segment_len`,
/// applying the WAL Spec §6.2–§6.3 torn-vs-corrupt rule at every
/// validation failure. Does not read or validate the segment header
/// itself (WAL Spec §2.2) — callers do that first via
/// `format::decode_segment_header`, since a bad header is corruption
/// unconditionally (§6.2 step 1), never something this function's
/// tail-position logic should reason about.
///
/// The tail-eligibility test used throughout: a validation failure at
/// `offset` is only ever classified as a torn write if the frame's own
/// *claimed* extent (`offset + FRAME_HEADER_LEN + length`) lands exactly
/// at `segment_len` — i.e., nothing at all follows it in the file. WAL
/// Spec §6.3: "A torn write can only ever be the very last
/// incomplete-or-invalid frame in the very last segment being replayed."
/// Any bytes at all following a bad frame disqualify it from that
/// leniency, regardless of whether those following bytes themselves parse
/// as anything — a real crash can only ever leave the single most-recent
/// `write()` call torn, so this shape (more bytes after a bad frame)
/// should never arise from an honest crash in the first place; treating it
/// as corruption is the fail-closed choice the spec's design goals call
/// for.
pub fn walk_segment<R: Read + Seek>(
    reader: &mut R,
    segment_len: u64,
    max_record_len: usize,
) -> io::Result<WalkOutcome> {
    let mut offset = SEGMENT_HEADER_LEN as u64;
    let mut out = WalkOutcome::default();

    loop {
        let remaining = segment_len.saturating_sub(offset);
        if remaining == 0 {
            break; // clean end of segment
        }
        if remaining < FRAME_HEADER_LEN as u64 {
            out.truncated = true; // torn write mid frame-header (§6.2 step 2)
            break;
        }

        reader.seek(SeekFrom::Start(offset))?;
        let mut header_buf = [0u8; FRAME_HEADER_LEN];
        reader.read_exact(&mut header_buf)?;
        // Both reads are over a fixed 4-byte sub-slice of an 8-byte array
        // we just filled ourselves, so they cannot fail — but `read_u32_le`
        // is `Result`-returning everywhere else in the crate (Group 2.2),
        // so match that shape here too via `expect` on a provably-Ok value
        // rather than silently discarding a `Result`.
        let length =
            read_u32_le(&header_buf[0..4]).expect("4-byte slice of a fixed local buffer") as u64;
        let stored_crc =
            read_u32_le(&header_buf[4..8]).expect("4-byte slice of a fixed local buffer");

        // §6.2 step 3 + §2.6: an out-of-range length is never used to size
        // a read, so it can never be trusted to compute this frame's
        // claimed extent either. The tail test for *this* failure mode is
        // therefore "does anything at all follow the 8-byte frame header"
        // — the shape a crash mid-header-write leaves — not "does the
        // (untrustworthy) declared length reach exactly to EOF."
        if length as usize > max_record_len || length > u32::MAX as u64 {
            // `saturating_add`, matching this function's other arithmetic
            // on `offset`/`segment_len`, even though `offset` is already
            // provably bounded by a prior `checked_add` in this same loop
            // (Group 3.3) — kept checked/saturating here too rather than
            // relying on that induction argument at this specific call
            // site as well, per the Non-Negotiable Security bar's blanket
            // "never raw +/* on a corruption-adjacent value" rule. A
            // saturated result only ever makes `header_is_tail` false,
            // never falsely true, so this can't weaken the classification.
            let header_is_tail = segment_len == offset.saturating_add(FRAME_HEADER_LEN as u64);
            classify_failure(
                &mut out,
                offset,
                header_is_tail,
                format!(
                    "declared length {length} at offset {offset} exceeds MAX_RECORD_LEN ({max_record_len})"
                ),
            );
            break;
        }

        // From here on `length` is validated (<= max_record_len), so it's
        // safe to use in computing this frame's claimed extent — the tail
        // test for the CRC-mismatch and decode-failure branches below.
        let claimed_end = offset
            .checked_add(FRAME_HEADER_LEN as u64)
            .and_then(|v| v.checked_add(length));
        let is_tail_frame = claimed_end == Some(segment_len);

        let remaining_after_header = segment_len.saturating_sub(offset + FRAME_HEADER_LEN as u64);
        if remaining_after_header < length {
            // Physically cannot have anything after it either — always the
            // tail by construction, always torn (§6.2 step 4).
            out.truncated = true;
            break;
        }

        let mut body = vec![0u8; length as usize]; // bounded by max_record_len, validated above
        reader.read_exact(&mut body)?;
        let computed_crc = crc32c::crc32c(&body);

        if computed_crc != stored_crc {
            classify_failure(
                &mut out,
                offset,
                is_tail_frame,
                format!(
                    "CRC mismatch at offset {offset}: stored={stored_crc:#010x} computed={computed_crc:#010x}"
                ),
            );
            break;
        }

        match decode_wal_body(&body) {
            Ok((seq, op)) => {
                // Group 3.3: `claimed_end` is `Some` for any *realistic*
                // segment (length already validated <= max_record_len,
                // offset <= segment_len — the checked_add pair can't
                // overflow u64 in practice), but a hand-crafted or
                // maliciously corrupted file could still, in principle,
                // present an `offset` large enough to make the sum
                // overflow even for an otherwise length-valid frame. That
                // must fail closed as corruption, not panic via `.expect`.
                let Some(next_offset) = claimed_end else {
                    out.corrupted = true;
                    out.corruption_offset = Some(offset);
                    out.corruption_detail =
                        Some(format!("frame extent overflow at offset {offset}"));
                    break;
                };
                out.records.push((seq, op));
                offset = next_offset;
            }
            Err(e) => {
                // Body was fully present and CRC-valid, but internally
                // malformed. Cannot happen from a conformant writer — the
                // CRC guards these exact bytes — so if it ever does, it is
                // corruption, not a torn write, by the same tail-position
                // reasoning as a CRC mismatch.
                classify_failure(
                    &mut out,
                    offset,
                    is_tail_frame,
                    format!("frame at offset {offset} passed length/CRC but failed to decode: {e}"),
                );
                break;
            }
        }
    }

    out.valid_end_offset = offset;
    Ok(out)
}

fn classify_failure(out: &mut WalkOutcome, offset: u64, is_tail_frame: bool, detail: String) {
    if is_tail_frame {
        out.truncated = true;
    } else {
        out.corrupted = true;
        out.corruption_offset = Some(offset);
        out.corruption_detail = Some(detail);
    }
}

/// Validates a segment's header (never a torn-write case, WAL Spec §6.2
/// step 1) and, if valid, walks its frames. Convenience wrapper combining
/// `format::decode_segment_header` and `walk_segment` for the common case
/// of scanning one whole segment file/buffer from byte 0.
pub fn walk_full_segment<R: Read + Seek>(
    reader: &mut R,
    segment_len: u64,
    expected_segment_id: u64,
    max_record_len: usize,
) -> io::Result<Result<WalkOutcome, crate::error::EngineError>> {
    if segment_len < SEGMENT_HEADER_LEN as u64 {
        return Ok(Err(crate::error::EngineError::Corruption {
            detail: format!(
                "segment {expected_segment_id} is {segment_len} bytes, shorter than the \
                 {SEGMENT_HEADER_LEN}-byte header"
            ),
        }));
    }
    reader.seek(SeekFrom::Start(0))?;
    let mut header_buf = [0u8; SEGMENT_HEADER_LEN];
    reader.read_exact(&mut header_buf)?;
    if let Err(e) = decode_segment_header(&header_buf, expected_segment_id) {
        return Ok(Err(e));
    }
    Ok(Ok(walk_segment(reader, segment_len, max_record_len)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::file_io::{MemFile, WalFile};
    use crate::wal::format::{encode_segment_header, DEFAULT_MAX_RECORD_LEN};
    use crate::wal::ops::{encode_wal_frame, WalOp};
    use std::io::Write;

    fn segment_with_records(records: &[(u64, WalOp<'_>)]) -> MemFile {
        let mut f = MemFile::new();
        f.write_all(&encode_segment_header(1)).unwrap();
        for (seq, op) in records {
            let frame = encode_wal_frame(*seq, op.clone(), DEFAULT_MAX_RECORD_LEN).unwrap();
            f.write_all(&frame).unwrap();
        }
        f
    }

    #[test]
    fn clean_multi_record_segment() {
        let f = segment_with_records(&[
            (
                1,
                WalOp::Put {
                    key: b"a",
                    value: b"1",
                },
            ),
            (
                2,
                WalOp::Put {
                    key: b"b",
                    value: b"2",
                },
            ),
            (3, WalOp::Delete { key: b"a" }),
        ]);
        let len = f.size().unwrap();
        let outcome = walk_full_segment(&mut f.clone(), len, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.records.len(), 3);
        assert!(!outcome.truncated);
        assert!(!outcome.corrupted);
        assert_eq!(outcome.valid_end_offset, len);
    }

    #[test]
    fn torn_mid_frame_header() {
        let mut f = segment_with_records(&[(1, WalOp::Delete { key: b"a" })]);
        let full_len = f.size().unwrap();
        // Truncate to leave 3 bytes of the next (never-written) frame's
        // header — simulate a crash mid next-append.
        let valid_len = full_len; // end of the one valid record
        f.set_len(valid_len + 3).unwrap();
        let total_len = valid_len + 3;
        let outcome = walk_full_segment(&mut f, total_len, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.records.len(), 1);
        assert!(outcome.truncated);
        assert!(!outcome.corrupted);
        assert_eq!(outcome.valid_end_offset, valid_len);
    }

    #[test]
    fn torn_mid_body() {
        let mut f = segment_with_records(&[(1, WalOp::Delete { key: b"a" })]);
        let valid_len = f.size().unwrap();
        let second = encode_wal_frame(
            2,
            WalOp::Put {
                key: b"bbbb",
                value: b"cccc",
            },
            DEFAULT_MAX_RECORD_LEN,
        )
        .unwrap();
        f.write_all(&second[..second.len() - 3]).unwrap(); // drop last 3 body bytes
        let total_len = f.size().unwrap();
        let outcome = walk_full_segment(&mut f, total_len, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.records.len(), 1);
        assert!(outcome.truncated);
        assert!(!outcome.corrupted);
        assert_eq!(outcome.valid_end_offset, valid_len);
    }

    #[test]
    fn torn_exactly_at_frame_boundary_is_not_flagged_truncated_incorrectly() {
        let f = segment_with_records(&[
            (1, WalOp::Delete { key: b"a" }),
            (2, WalOp::Delete { key: b"b" }),
        ]);
        let len = f.size().unwrap();
        let outcome = walk_full_segment(&mut f.clone(), len, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.records.len(), 2);
        assert!(!outcome.truncated);
        assert!(!outcome.corrupted);
    }

    #[test]
    fn non_tail_corruption_is_never_silently_truncated() {
        let f = segment_with_records(&[
            (
                1,
                WalOp::Put {
                    key: b"a",
                    value: b"1",
                },
            ),
            (
                2,
                WalOp::Put {
                    key: b"b",
                    value: b"2",
                },
            ),
            (
                3,
                WalOp::Put {
                    key: b"c",
                    value: b"3",
                },
            ),
        ]);
        // Flip a byte inside record 2's body. Its offset: header(24) +
        // frame1_len + FRAME_HEADER_LEN(8) + 8(seq) + 1(op) + 4(key_len) +
        // 1(key) -> land inside its value byte.
        let frame1_len = encode_wal_frame(
            1,
            WalOp::Put {
                key: b"a",
                value: b"1",
            },
            DEFAULT_MAX_RECORD_LEN,
        )
        .unwrap()
        .len();
        let record2_value_offset =
            SEGMENT_HEADER_LEN + frame1_len + FRAME_HEADER_LEN + 8 + 1 + 4 + 1 + 4;
        f.corrupt_byte_at(record2_value_offset, b'X');
        let len = f.size().unwrap();
        let outcome = walk_full_segment(&mut f.clone(), len, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome.records.len(),
            1,
            "only record 1 precedes the corruption"
        );
        assert!(!outcome.truncated, "must not be mistaken for a torn tail");
        assert!(outcome.corrupted, "must be flagged corrupted");
    }

    #[test]
    fn max_record_len_violation_at_tail_is_torn_not_corrupt() {
        let mut f = MemFile::new();
        f.write_all(&encode_segment_header(1)).unwrap();
        // Hand-craft a frame header declaring an absurd length, with
        // nothing after it in the file (classic corrupted-length-at-EOF
        // shape, indistinguishable from a torn header write).
        f.write_all(&u32::MAX.to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        let len = f.size().unwrap();
        let outcome = walk_full_segment(&mut f, len, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .unwrap();
        assert!(outcome.truncated);
        assert!(!outcome.corrupted);
        assert_eq!(outcome.valid_end_offset, SEGMENT_HEADER_LEN as u64);
    }

    #[test]
    fn max_record_len_violation_not_at_tail_is_corrupt() {
        let mut f = segment_with_records(&[(1, WalOp::Delete { key: b"a" })]);
        // Append a bogus oversized-length header, then MORE bytes after
        // it, so it cannot be the tail.
        f.write_all(&u32::MAX.to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.write_all(b"trailing garbage").unwrap();
        let len = f.size().unwrap();
        let outcome = walk_full_segment(&mut f, len, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.records.len(), 1);
        assert!(!outcome.truncated);
        assert!(outcome.corrupted);
    }

    #[test]
    fn empty_segment_after_header() {
        let mut f = MemFile::new();
        f.write_all(&encode_segment_header(1)).unwrap();
        let len = f.size().unwrap();
        let outcome = walk_full_segment(&mut f, len, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .unwrap();
        assert!(outcome.records.is_empty());
        assert!(!outcome.truncated);
        assert!(!outcome.corrupted);
        assert_eq!(outcome.valid_end_offset, SEGMENT_HEADER_LEN as u64);
    }

    #[test]
    fn short_file_is_corruption_not_panic() {
        let mut f = MemFile::new();
        f.write_all(&[0u8; 10]).unwrap(); // shorter than the 24-byte header
        let result = walk_full_segment(&mut f, 10, 1, DEFAULT_MAX_RECORD_LEN).unwrap();
        assert!(result.is_err());
    }
}
