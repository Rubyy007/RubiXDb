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
        // A tail that is zero-filled to the end of the segment (length extended, data never written) is a torn tail whatever
        // its length: no real frame is all zero (its body holds at least a seq and an op byte).
        if header_buf == [0u8; FRAME_HEADER_LEN] {
            let mut left = remaining - FRAME_HEADER_LEN as u64;
            let mut zero = true;
            let mut chunk = [0u8; 4096];
            while left > 0 && zero {
                let want = left.min(chunk.len() as u64) as usize;
                reader.read_exact(&mut chunk[..want])?;
                zero = chunk[..want].iter().all(|b| *b == 0);
                left -= want as u64;
            }
            if zero {
                out.truncated = true;
                break;
            }
            reader.seek(SeekFrom::Start(offset + FRAME_HEADER_LEN as u64))?;
        }
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
            classify_failure_or_lost_sector(
                reader,
                &mut out,
                offset,
                header_is_tail,
                segment_len,
                max_record_len,
                format!(
                    "declared length {length} at offset {offset} exceeds MAX_RECORD_LEN ({max_record_len})"
                ),
            )?;
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
            classify_failure_or_lost_sector(
                reader,
                &mut out,
                offset,
                is_tail_frame,
                segment_len,
                max_record_len,
                format!(
                    "CRC mismatch at offset {offset}: stored={stored_crc:#010x} computed={computed_crc:#010x}"
                ),
            )?;
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
                classify_failure_or_lost_sector(
                    reader,
                    &mut out,
                    offset,
                    is_tail_frame,
                    segment_len,
                    max_record_len,
                    format!("frame at offset {offset} passed length/CRC but failed to decode: {e}"),
                )?;
                break;
            }
        }
    }

    out.valid_end_offset = offset;
    Ok(out)
}

/// The smallest sector size the model and real devices use; a lost larger sector satisfies the first-sector test a fortiori.
const LOST_SECTOR_GRANULE: u64 = 512;

/// Upper bound on the CRC input this scan may hash (fails closed beyond it).
const LOST_SECTOR_CRC_BUDGET: u64 = 64 * 1024 * 1024;

/// ADR-TORN-TAIL-LOST-SECTOR-01. `offset` is a frame boundary at which a frame failed validation as something other than the
/// provable tail. True iff the tail is the remainder of ONE record whose first sector was lost: (1) the bytes up to the
/// next sector boundary are all zero, (2) the tail is no longer than one maximum-size frame, (3) no complete valid frame
/// starts anywhere behind the hole. Anything else keeps the old classification (corruption), so a hole in front of readable
/// data is never truncated away.
fn lost_first_sector_tail<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    segment_len: u64,
    max_record_len: usize,
) -> io::Result<bool> {
    let tail_len = segment_len.saturating_sub(offset);
    if tail_len > (FRAME_HEADER_LEN + max_record_len) as u64 {
        return Ok(false);
    }
    // The first sector is the bytes from `offset` to the next sector boundary (a header that straddles the boundary is
    // therefore only partly zero).
    let zero_end = (offset / LOST_SECTOR_GRANULE + 1) * LOST_SECTOR_GRANULE;
    if zero_end >= segment_len {
        return Ok(false);
    }
    reader.seek(SeekFrom::Start(offset))?;
    let mut tail = vec![0u8; tail_len as usize]; // bounded by the check above
    reader.read_exact(&mut tail)?;
    let hole = (zero_end - offset) as usize;
    if tail[..hole].iter().any(|b| *b != 0) {
        return Ok(false);
    }
    let mut budget = LOST_SECTOR_CRC_BUDGET;
    let mut p = hole;
    while p + FRAME_HEADER_LEN < tail.len() {
        let length = u32::from_le_bytes([tail[p], tail[p + 1], tail[p + 2], tail[p + 3]]) as usize;
        if length > 0 && length <= max_record_len && p + FRAME_HEADER_LEN + length <= tail.len() {
            let stored = u32::from_le_bytes([tail[p + 4], tail[p + 5], tail[p + 6], tail[p + 7]]);
            let body = &tail[p + FRAME_HEADER_LEN..p + FRAME_HEADER_LEN + length];
            if budget < length as u64 {
                return Ok(false); // fail closed
            }
            budget -= length as u64;
            if crc32c::crc32c(body) == stored && decode_wal_body(body).is_ok() {
                return Ok(false); // a readable frame behind the hole: mid-file damage
            }
        }
        p += 1;
    }
    Ok(true)
}

/// Corruption is only declared after the lost-first-sector rule (ADR-TORN-TAIL-LOST-SECTOR-01) has been given the chance to
/// call the failure a torn tail.
fn classify_failure_or_lost_sector<R: Read + Seek>(
    reader: &mut R,
    out: &mut WalkOutcome,
    offset: u64,
    is_tail_frame: bool,
    segment_len: u64,
    max_record_len: usize,
    detail: String,
) -> io::Result<()> {
    if !is_tail_frame && lost_first_sector_tail(reader, offset, segment_len, max_record_len)? {
        out.truncated = true;
    } else {
        classify_failure(out, offset, is_tail_frame, detail);
    }
    Ok(())
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

    // ---- ADR-TORN-TAIL-LOST-SECTOR-01 -------------------------------------------------------------------------------

    fn put_frame(seq: u64, key: &[u8], value_len: usize) -> Vec<u8> {
        let value = vec![0xAB; value_len];
        encode_wal_frame(
            seq,
            WalOp::Put { key, value: &value },
            DEFAULT_MAX_RECORD_LEN,
        )
        .unwrap()
    }

    /// Segment header, one acknowledged record whose size shifts the next frame start, then a big record. Returns the bytes
    /// and the start offset of the big record.
    fn lost_sector_image(pad: usize) -> (Vec<u8>, usize) {
        let mut b = encode_segment_header(1).to_vec();
        b.extend(put_frame(1, b"k", pad));
        let o = b.len();
        b.extend(put_frame(2, b"big", 1500));
        (b, o)
    }

    fn walk(bytes: &[u8]) -> WalkOutcome {
        let mut c = std::io::Cursor::new(bytes.to_vec());
        walk_segment(&mut c, bytes.len() as u64, DEFAULT_MAX_RECORD_LEN).unwrap()
    }

    #[test]
    fn lost_first_sector_is_a_torn_tail_at_every_alignment() {
        let mut aligned = 0;
        for pad in 0..1100usize {
            let (mut b, o) = lost_sector_image(pad);
            let boundary = (o / 512 + 1) * 512;
            for x in &mut b[o..boundary] {
                *x = 0;
            }
            let out = walk(&b);
            assert!(
                out.truncated
                    && !out.corrupted
                    && out.records.len() == 1
                    && out.valid_end_offset == o as u64,
                "pad={pad} o={o}: truncated={} corrupted={} records={} end={}",
                out.truncated,
                out.corrupted,
                out.records.len(),
                out.valid_end_offset
            );
            if o % 512 == 0 {
                aligned += 1;
            }
        }
        assert!(aligned > 0, "no sector-aligned record start was exercised");
    }

    #[test]
    fn lost_first_sector_followed_by_a_valid_frame_is_corruption() {
        let (mut b, o) = lost_sector_image(10);
        let boundary = (o / 512 + 1) * 512;
        for x in &mut b[o..boundary] {
            *x = 0;
        }
        b.extend(put_frame(3, b"after", 5)); // a complete valid frame behind the hole
        let out = walk(&b);
        assert!(out.corrupted && !out.truncated, "{out:?}");
    }

    #[test]
    fn zeroed_header_with_intact_first_sector_stays_corruption() {
        let (mut b, o) = lost_sector_image(10);
        for x in &mut b[o..o + 8] {
            *x = 0;
        }
        let out = walk(&b);
        assert!(out.corrupted && !out.truncated, "{out:?}");
    }

    #[test]
    fn a_hole_before_more_than_one_maximum_frame_is_not_a_lost_sector() {
        // tail longer than 8 + max_record_len: cannot be one record, old classification (corruption) applies
        let (mut b, o) = lost_sector_image(10);
        let boundary = (o / 512 + 1) * 512;
        for x in &mut b[o..boundary] {
            *x = 0;
        }
        let mut c = std::io::Cursor::new(b.clone());
        let tiny_max = 256usize; // the tail (1.5 KB) exceeds 8 + 256
        let out = walk_segment(&mut c, b.len() as u64, tiny_max).unwrap();
        assert!(!out.records.is_empty() || out.corrupted || out.truncated);
        assert!(
            !(out.truncated && out.valid_end_offset == o as u64 && !out.corrupted),
            "{out:?}"
        );
    }
}
