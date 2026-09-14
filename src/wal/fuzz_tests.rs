//! WAL Spec §11 test #14: the ≥1,000-run randomized fuzz/property test
//! over the recovery algorithm's core invariant. Approved by the user:
//! `proptest`, dev-dependency only (see `ARCHITECTURE.md`).
//!
//! Runs entirely against the in-memory `MemFile`/`walk_full_segment`
//! machinery (never real files) so that 1,000+ iterations stay fast and
//! deterministic-per-seed — see `file_io::MemFile`'s doc comment for why
//! this backend exists.

use std::io::Write;

use proptest::collection::vec as pvec;
use proptest::prelude::*;

use crate::wal::file_io::{MemFile, WalFile};
use crate::wal::format::{encode_segment_header, DEFAULT_MAX_RECORD_LEN};
use crate::wal::ops::{encode_wal_frame, WalOp, WalOpOwned};
use crate::wal::recovery::walk_full_segment;

/// A small, `proptest`-generatable stand-in for one WAL operation, kept
/// separate from `WalOp` because `WalOp` borrows and `proptest::Strategy`
/// values need to be owned.
#[derive(Debug, Clone)]
enum FuzzOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
    CheckpointMarker { flushed_through_seq: u64 },
}

fn fuzz_op_strategy() -> impl Strategy<Value = FuzzOp> {
    prop_oneof![
        (pvec(any::<u8>(), 0..16), pvec(any::<u8>(), 0..16))
            .prop_map(|(key, value)| FuzzOp::Put { key, value }),
        pvec(any::<u8>(), 0..16).prop_map(|key| FuzzOp::Delete { key }),
        any::<u64>().prop_map(|flushed_through_seq| FuzzOp::CheckpointMarker {
            flushed_through_seq
        }),
    ]
}

fn encode(seq: u64, op: &FuzzOp) -> Vec<u8> {
    let wal_op = match op {
        FuzzOp::Put { key, value } => WalOp::Put { key, value },
        FuzzOp::Delete { key } => WalOp::Delete { key },
        FuzzOp::CheckpointMarker {
            flushed_through_seq,
        } => WalOp::CheckpointMarker {
            flushed_through_seq: *flushed_through_seq,
        },
    };
    encode_wal_frame(seq, wal_op, DEFAULT_MAX_RECORD_LEN)
        .expect("fuzz payloads stay well under MAX_RECORD_LEN")
}

fn as_owned(op: &FuzzOp) -> WalOpOwned {
    match op {
        FuzzOp::Put { key, value } => WalOpOwned::Put {
            key: key.clone(),
            value: value.clone(),
        },
        FuzzOp::Delete { key } => WalOpOwned::Delete { key: key.clone() },
        FuzzOp::CheckpointMarker {
            flushed_through_seq,
        } => WalOpOwned::CheckpointMarker {
            flushed_through_seq: *flushed_through_seq,
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// The core WAL correctness invariant (WAL Spec §11 test #14): for any
    /// sequence of appended records and any crash point (truncation
    /// offset), recovery returns exactly the longest prefix of the
    /// originally-appended records whose full frame lies at or before the
    /// truncation point — never a suffix, never a gap, never an
    /// out-of-order `seq`, never a record that was never appended, and
    /// never a panic.
    #[test]
    fn recovery_returns_exactly_the_durable_prefix(
        ops in pvec(fuzz_op_strategy(), 0..30),
        cut_fraction in 0.0f64..=1.0f64,
    ) {
        let mut f = MemFile::new();
        f.write_all(&encode_segment_header(1)).unwrap();

        // Record each original record's (seq, owned-op, frame-end-offset)
        // so the expected prefix can be recomputed from the chosen cut
        // point without re-deriving anything the code under test computes.
        let mut originals: Vec<(u64, WalOpOwned, u64)> = Vec::new();
        let mut offset = WalFile::size(&f).unwrap();
        for (i, op) in ops.iter().enumerate() {
            let seq = (i as u64) + 1;
            let frame = encode(seq, op);
            f.write_all(&frame).unwrap();
            offset += frame.len() as u64;
            originals.push((seq, as_owned(op), offset));
        }

        let full_len = WalFile::size(&f).unwrap();
        let header_len = crate::wal::format::SEGMENT_HEADER_LEN as u64;
        // Cut anywhere from "right after the header" to "the full file",
        // covering torn-mid-header, torn-mid-body, and clean-boundary
        // crash points uniformly.
        let cut_at = header_len + ((full_len - header_len) as f64 * cut_fraction).round() as u64;
        WalFile::set_len(&f, cut_at).unwrap();

        let expected: Vec<(u64, WalOpOwned)> = originals
            .into_iter()
            .filter(|&(_, _, frame_end)| frame_end <= cut_at)
            .map(|(seq, op, _)| (seq, op))
            .collect();

        let outcome = walk_full_segment(&mut f, cut_at, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .expect("header is always intact: never truncated below header_len by this harness");

        // Corruption should never be *reachable* from a pure truncation
        // (no byte content was altered, only file length) — if it is
        // triggered, that is itself the invariant violation.
        prop_assert!(!outcome.corrupted, "an honest truncation must never be classified as corruption");
        prop_assert_eq!(outcome.records, expected);
    }

    /// Group 7.1: after building a valid multi-record segment, flip one
    /// random byte at a random offset *excluding the header* (header
    /// corruption is a separate, unconditional-corruption code path
    /// already covered by unit tests) and assert the general prefix
    /// invariant: recovery's returned `records` is always an exact prefix
    /// of what was originally written — never a suffix, never a gap, no
    /// value substitutions — and if that prefix is shorter than the full
    /// original set, *something* must explain the missing tail
    /// (`corrupted` or `truncated`), never silence.
    #[test]
    fn recovery_returns_prefix_under_random_corruption(
        ops in pvec(fuzz_op_strategy(), 1..20),
        flip_fraction in 0.0f64..1.0f64,
        replacement_byte in any::<u8>(),
    ) {
        let mut f = MemFile::new();
        f.write_all(&encode_segment_header(1)).unwrap();

        let mut originals: Vec<(u64, WalOpOwned)> = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            let seq = (i as u64) + 1;
            f.write_all(&encode(seq, op)).unwrap();
            originals.push((seq, as_owned(op)));
        }

        let header_len = crate::wal::format::SEGMENT_HEADER_LEN as u64;
        let full_len = WalFile::size(&f).unwrap();
        prop_assume!(full_len > header_len);

        // Exclude the header from the corruption target — corrupted by
        // construction below, restricted to [header_len, full_len).
        let span = full_len - header_len;
        let flip_at = header_len + ((span as f64 * flip_fraction) as u64).min(span - 1);
        let original_byte = {
            let snapshot = f.snapshot();
            snapshot[flip_at as usize]
        };
        // Guarantee an actual change, so this case can never coincidentally
        // be a no-op flip.
        let new_byte = if replacement_byte == original_byte {
            replacement_byte.wrapping_add(1)
        } else {
            replacement_byte
        };
        f.corrupt_byte_at(flip_at as usize, new_byte);

        let outcome = walk_full_segment(&mut f, full_len, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .expect("header was excluded from corruption, so it must still decode");

        prop_assert!(
            outcome.records.len() <= originals.len(),
            "must never return more records than were ever appended"
        );
        prop_assert_eq!(
            &outcome.records[..],
            &originals[..outcome.records.len()],
            "returned records must be an exact prefix, never a gap or substitution"
        );
        if outcome.records.len() < originals.len() {
            prop_assert!(
                outcome.corrupted || outcome.truncated,
                "a short prefix must always be explained by `corrupted` or `truncated`"
            );
        }
    }

    /// Group 7.1: truncate the *last* frame at a point strictly inside its
    /// fixed 8-byte frame header (so the "insufficient bytes for even a
    /// header" check fires unconditionally — WAL Spec §6.2 step 1/step 2 —
    /// regardless of byte content) and additionally overwrite the first 4
    /// bytes (the `length` field) with random data, simulating a partial
    /// write whose header bytes are themselves garbage. Classification
    /// must still be a clean torn tail: this is exactly the scenario the
    /// spec's design goals call "expected, not alarming," and it must not
    /// depend on what particular garbage bytes a partial write happened to
    /// leave behind.
    #[test]
    fn recovery_returns_prefix_under_partial_write(
        ops in pvec(fuzz_op_strategy(), 1..20),
        header_bytes_present in 4u64..8,
        garbage in proptest::array::uniform4(any::<u8>()),
    ) {
        let mut f = MemFile::new();
        f.write_all(&encode_segment_header(1)).unwrap();

        let mut originals: Vec<(u64, WalOpOwned)> = Vec::new();
        let mut last_frame_start = WalFile::size(&f).unwrap();
        for (i, op) in ops.iter().enumerate() {
            let seq = (i as u64) + 1;
            last_frame_start = WalFile::size(&f).unwrap();
            f.write_all(&encode(seq, op)).unwrap();
            originals.push((seq, as_owned(op)));
        }

        let cut_at = last_frame_start + header_bytes_present;
        f.corrupt_byte_at(last_frame_start as usize, garbage[0]);
        f.corrupt_byte_at(last_frame_start as usize + 1, garbage[1]);
        f.corrupt_byte_at(last_frame_start as usize + 2, garbage[2]);
        f.corrupt_byte_at(last_frame_start as usize + 3, garbage[3]);
        WalFile::set_len(&f, cut_at).unwrap();

        let outcome = walk_full_segment(&mut f, cut_at, 1, DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .expect("header is untouched by this test");

        prop_assert!(outcome.truncated, "fewer than 8 bytes of a frame header must always be torn");
        prop_assert!(!outcome.corrupted);
        prop_assert_eq!(&outcome.records[..], &originals[..originals.len() - 1]);
    }
}

/// Group 7.1: `walk_full_segment` must never panic on *any* byte string,
/// however malformed — not just plausible corruptions/truncations of a
/// once-valid segment (the property tests above), but arbitrary noise. A
/// fixed-seed deterministic PRNG (not `proptest`'s own machinery, and no
/// new dependency — see the user's spec for this test) generates 10,000
/// random byte strings of length `0..512`; every one of them, however it
/// resolves, must fit one of exactly three shapes: an I/O error (`Err`,
/// unreachable for an in-memory buffer in practice but part of the type),
/// a structural/header decode failure (`Ok(Err(_))`), or a successful walk
/// (`Ok(Ok(_))`, itself possibly reporting `corrupted`/`truncated`).
#[test]
fn recovery_never_panics_on_arbitrary_noise() {
    struct XorShift64(u64);
    impl XorShift64 {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn next_len(&mut self, max_exclusive: u64) -> usize {
            (self.next_u64() % max_exclusive) as usize
        }
        fn next_byte(&mut self) -> u8 {
            (self.next_u64() & 0xFF) as u8
        }
    }

    let mut rng = XorShift64(0x5EED_C0FF_EE15_5EED);
    for _ in 0..10_000 {
        let len = rng.next_len(512);
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            bytes.push(rng.next_byte());
        }

        let mut f = MemFile::new();
        f.write_all(&bytes).unwrap();
        let segment_len = bytes.len() as u64;

        match walk_full_segment(&mut f, segment_len, 1, DEFAULT_MAX_RECORD_LEN) {
            Ok(Ok(_outcome)) => {}
            Ok(Err(_engine_err)) => {}
            Err(_io_err) => {}
        }
    }
}
