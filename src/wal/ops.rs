//! `WalOp`/`WalOpOwned` and their `body` encoding, per WAL Spec §2.4 and
//! §5. Built on the generic frame primitives in `wal::format`.

use crate::error::{EngineError, Result};
use crate::wal::format::{
    self, read_len_prefixed, read_u64_le, write_len_prefixed, OP_CHECKPOINT_MARKER, OP_DELETE,
    OP_ENGINE_SWITCH, OP_PUT,
};

/// One WAL operation, borrowing its key/value bytes from the caller —
/// WAL Spec §5.
#[derive(Debug, Clone, Copy)]
pub enum WalOp<'a> {
    Put { key: &'a [u8], value: &'a [u8] },
    Delete { key: &'a [u8] },
    CheckpointMarker { flushed_through_seq: u64 },
}

/// The owned form of `WalOp`, returned by recovery (WAL Spec §5's
/// `WalReplayResult::records: Vec<(u64, WalOpOwned)>`), since replayed
/// records must outlive the buffer they were decoded from. Also used by
/// Phase 2's `execution::WriteWorkerPool` (`PHASE2_WORKER_POOL_
/// ARCHITECTURE.md`) to carry a request's payload across the submission
/// queue to whichever worker thread eventually processes it — a borrowed
/// `WalOp<'a>` cannot cross that boundary, since the submitting caller's
/// own stack frame is not guaranteed to outlive the wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalOpOwned {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
    CheckpointMarker { flushed_through_seq: u64 },
}

impl WalOpOwned {
    /// Re-borrows this owned op as a `WalOp<'_>` — the form `GroupCommitter::
    /// append`/`append_durable` actually take. No copy: the returned
    /// `WalOp` borrows this value's own `Vec<u8>` buffers directly, so a
    /// payload submitted once (copied from the caller's original slice
    /// into this `WalOpOwned` at submission time — unavoidable, since the
    /// caller's own stack frame need not outlive the wait) is never
    /// copied a second time on its way into the WAL.
    pub fn as_wal_op(&self) -> WalOp<'_> {
        match self {
            WalOpOwned::Put { key, value } => WalOp::Put { key, value },
            WalOpOwned::Delete { key } => WalOp::Delete { key },
            WalOpOwned::CheckpointMarker {
                flushed_through_seq,
            } => WalOp::CheckpointMarker {
                flushed_through_seq: *flushed_through_seq,
            },
        }
    }
}

/// Encodes one full WAL frame for `seq`/`op`, per WAL Spec §2.3/§2.4.
/// Takes `op` by value (it is `Copy` — two slice references or one `u64`)
/// to mirror the `Wal::append(&mut self, op: WalOp)` trait signature
/// exactly, per WAL Spec §5.
///
/// Group 2.3: the op-body encoder is fallible and its errors propagate
/// straight out of `format::encode_frame` — an oversized key/value is
/// rejected at the exact point it's discovered (`write_len_prefixed`'s own
/// per-field check), never by emitting a deliberately-invalid on-disk
/// length and hoping a *different*, later check catches it before the
/// frame is written anywhere.
pub fn encode_wal_frame(seq: u64, op: WalOp<'_>, max_record_len: usize) -> Result<Vec<u8>> {
    format::encode_frame(seq, max_record_len, move |out| match op {
        WalOp::Put { key, value } => {
            write_len_prefixed(out, key, max_record_len)?;
            write_len_prefixed(out, value, max_record_len)?;
            Ok(OP_PUT)
        }
        WalOp::Delete { key } => {
            write_len_prefixed(out, key, max_record_len)?;
            Ok(OP_DELETE)
        }
        WalOp::CheckpointMarker {
            flushed_through_seq,
        } => {
            out.extend_from_slice(&flushed_through_seq.to_le_bytes());
            Ok(OP_CHECKPOINT_MARKER)
        }
    })
}

/// Decodes a frame's `body` bytes (already CRC-validated by the caller)
/// into `(seq, WalOpOwned)`, per WAL Spec §2.4.
pub fn decode_wal_body(body: &[u8]) -> Result<(u64, WalOpOwned)> {
    if body.len() < 9 {
        return Err(EngineError::Corruption {
            detail: format!(
                "frame body too short for seq+op: {} bytes, need >= 9",
                body.len()
            ),
        });
    }
    let seq = read_u64_le(&body[0..8])?;
    let op_tag = body[8];
    let op_body = &body[9..];

    let op = match op_tag {
        OP_PUT => {
            let (key, rest) = read_len_prefixed(op_body)?;
            let (value, rest) = read_len_prefixed(rest)?;
            if !rest.is_empty() {
                return Err(EngineError::Corruption {
                    detail: format!("{} trailing byte(s) after PUT op_body", rest.len()),
                });
            }
            WalOpOwned::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }
        }
        OP_DELETE => {
            let (key, rest) = read_len_prefixed(op_body)?;
            if !rest.is_empty() {
                return Err(EngineError::Corruption {
                    detail: format!("{} trailing byte(s) after DELETE op_body", rest.len()),
                });
            }
            WalOpOwned::Delete { key: key.to_vec() }
        }
        OP_CHECKPOINT_MARKER => {
            if op_body.len() != 8 {
                return Err(EngineError::Corruption {
                    detail: format!(
                        "CHECKPOINT_MARKER op_body must be 8 bytes, got {}",
                        op_body.len()
                    ),
                });
            }
            WalOpOwned::CheckpointMarker {
                flushed_through_seq: read_u64_le(op_body)?,
            }
        }
        OP_ENGINE_SWITCH => {
            return Err(EngineError::Corruption {
                detail: "ENGINE_SWITCH op encountered before Phase 5 support exists".to_string(),
            });
        }
        other => {
            return Err(EngineError::Corruption {
                detail: format!("unknown WAL op byte {other}"),
            });
        }
    };
    Ok((seq, op))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::format::{read_u32_le, FRAME_HEADER_LEN};

    fn frame_body(frame: &[u8]) -> &[u8] {
        &frame[FRAME_HEADER_LEN..]
    }

    #[test]
    fn put_round_trip() {
        let op = WalOp::Put {
            key: b"k1",
            value: b"v1",
        };
        let frame = encode_wal_frame(7, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let (seq, decoded) = decode_wal_body(frame_body(&frame)).unwrap();
        assert_eq!(seq, 7);
        assert_eq!(
            decoded,
            WalOpOwned::Put {
                key: b"k1".to_vec(),
                value: b"v1".to_vec()
            }
        );
    }

    #[test]
    fn delete_round_trip() {
        let op = WalOp::Delete { key: b"k1" };
        let frame = encode_wal_frame(9, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let (seq, decoded) = decode_wal_body(frame_body(&frame)).unwrap();
        assert_eq!(seq, 9);
        assert_eq!(
            decoded,
            WalOpOwned::Delete {
                key: b"k1".to_vec()
            }
        );
    }

    #[test]
    fn checkpoint_marker_round_trip() {
        let op = WalOp::CheckpointMarker {
            flushed_through_seq: 12345,
        };
        let frame = encode_wal_frame(10, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let (seq, decoded) = decode_wal_body(frame_body(&frame)).unwrap();
        assert_eq!(seq, 10);
        assert_eq!(
            decoded,
            WalOpOwned::CheckpointMarker {
                flushed_through_seq: 12345
            }
        );
    }

    #[test]
    fn empty_value_put_round_trips() {
        let op = WalOp::Put {
            key: b"k",
            value: b"",
        };
        let frame = encode_wal_frame(1, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let (_, decoded) = decode_wal_body(frame_body(&frame)).unwrap();
        assert_eq!(
            decoded,
            WalOpOwned::Put {
                key: b"k".to_vec(),
                value: Vec::new()
            }
        );
    }

    #[test]
    fn decode_rejects_engine_switch() {
        let mut body = 1u64.to_le_bytes().to_vec();
        body.push(OP_ENGINE_SWITCH);
        let err = decode_wal_body(&body).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn decode_rejects_unknown_op() {
        let mut body = 1u64.to_le_bytes().to_vec();
        body.push(200);
        let err = decode_wal_body(&body).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn frame_header_encodes_correct_length_and_crc() {
        let op = WalOp::Delete { key: b"abc" };
        let frame = encode_wal_frame(1, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let declared_len = read_u32_le(&frame[0..4]).unwrap() as usize;
        assert_eq!(declared_len, frame.len() - FRAME_HEADER_LEN);
        let stored_crc = read_u32_le(&frame[4..8]).unwrap();
        assert_eq!(stored_crc, crc32c::crc32c(frame_body(&frame)));
    }

    /// Group 8 "definition of done" check: the on-disk wire format (WAL
    /// Spec §2) must be byte-for-byte identical before and after this
    /// hardening pass. Hardcodes the exact expected bytes for
    /// `encode_segment_header(42)` (WAL Spec §2.2, fully deterministic —
    /// no CRC involved) and for a sample `PUT` frame's every field except
    /// its CRC trailer (cross-checked against an independent
    /// `crc32c::crc32c` call instead, since hand-computing a CRC32C value
    /// by hand isn't practical) — a byte-shifting regression in either
    /// encoder would fail this test even if every higher-level round-trip
    /// test still happened to pass.
    #[test]
    fn wire_format_is_byte_for_byte_unchanged() {
        let header = format::encode_segment_header(42);
        let mut expected_header = Vec::new();
        expected_header.extend_from_slice(b"RBXWALv1"); // magic, WAL Spec §2.2
        expected_header.extend_from_slice(&1u32.to_le_bytes()); // format_version
        expected_header.extend_from_slice(&42u64.to_le_bytes()); // segment_id
        expected_header.extend_from_slice(&0u32.to_le_bytes()); // flags
        assert_eq!(
            header.len(),
            24,
            "WAL Spec §2.2: header is fixed at 24 bytes"
        );
        assert_eq!(&header[..], &expected_header[..]);

        let op = WalOp::Put {
            key: b"k1",
            value: b"v1",
        };
        let frame = encode_wal_frame(7, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();

        let mut expected_body = Vec::new();
        expected_body.extend_from_slice(&7u64.to_le_bytes()); // seq
        expected_body.push(OP_PUT); // op tag, WAL Spec §2.4
        expected_body.extend_from_slice(&2u32.to_le_bytes()); // key_len
        expected_body.extend_from_slice(b"k1"); // key
        expected_body.extend_from_slice(&2u32.to_le_bytes()); // val_len
        expected_body.extend_from_slice(b"v1"); // value
        assert_eq!(expected_body.len(), 21);

        let expected_length_field = expected_body.len() as u32; // WAL Spec §2.3
        let expected_crc = crc32c::crc32c(&expected_body);

        let mut expected_frame = Vec::new();
        expected_frame.extend_from_slice(&expected_length_field.to_le_bytes());
        expected_frame.extend_from_slice(&expected_crc.to_le_bytes());
        expected_frame.extend_from_slice(&expected_body);

        assert_eq!(frame.len(), 29, "8-byte frame header + 21-byte body");
        assert_eq!(&frame[..], &expected_frame[..]);
    }

    /// Group 2.3 regression test: a key larger than `max_record_len` must
    /// be rejected with `CapacityExceeded` — never silently truncated or
    /// emitted as a malformed frame with a bogus length sentinel.
    #[test]
    fn encode_rejects_oversized_key_with_capacity_exceeded() {
        let big_key = vec![0u8; 100];
        let op = WalOp::Put {
            key: &big_key,
            value: b"v",
        };
        let err = encode_wal_frame(1, op, 16).unwrap_err();
        assert!(matches!(err, EngineError::CapacityExceeded { .. }));
    }

    /// Group 2.4 regression test.
    #[test]
    fn decode_rejects_put_with_trailing_byte() {
        let op = WalOp::Put {
            key: b"k",
            value: b"v",
        };
        let frame = encode_wal_frame(1, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let mut body = frame_body(&frame).to_vec();
        body.push(0xFF);
        let err = decode_wal_body(&body).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    /// Group 2.4 regression test.
    #[test]
    fn decode_rejects_delete_with_trailing_byte() {
        let op = WalOp::Delete { key: b"k" };
        let frame = encode_wal_frame(1, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let mut body = frame_body(&frame).to_vec();
        body.push(0xFF);
        let err = decode_wal_body(&body).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    /// Group 2.4 regression test.
    #[test]
    fn decode_rejects_checkpoint_marker_with_wrong_length() {
        // 7 bytes: one short.
        let mut body = 1u64.to_le_bytes().to_vec();
        body.push(OP_CHECKPOINT_MARKER);
        body.extend_from_slice(&[0u8; 7]);
        let err = decode_wal_body(&body).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));

        // 9 bytes: one long.
        let mut body2 = 1u64.to_le_bytes().to_vec();
        body2.push(OP_CHECKPOINT_MARKER);
        body2.extend_from_slice(&[0u8; 9]);
        let err2 = decode_wal_body(&body2).unwrap_err();
        assert!(matches!(err2, EngineError::Corruption { .. }));
    }
}
