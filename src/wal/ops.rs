//! `WalOp`/`WalOpOwned` and their `body` encoding, per WAL Spec §2.4 and
//! §5. Built on the generic frame primitives in `wal::format`.

use crate::error::{EngineError, Result};
use crate::wal::format::{
    self, read_len_prefixed, read_u32_le, read_u64_le, write_len_prefixed, OP_CHECKPOINT_MARKER,
    OP_DELETE, OP_ENGINE_SWITCH, OP_GROUP, OP_PUT,
};

/// One mutation inside a `Group` (`write_batch`) frame —
/// `RELATIONAL ADR AMENDMENT 001` AA.2. Deliberately **not** a slice of
/// `WalOp<'a>` itself: a `GroupMember` has no `Group` variant of its
/// own and no `CheckpointMarker` variant, so nesting a group inside a
/// group, or embedding a checkpoint marker inside one, is a compile
/// error rather than a runtime check that could be forgotten.
#[derive(Debug, Clone, Copy)]
pub enum GroupMember<'a> {
    Put { key: &'a [u8], value: &'a [u8] },
    Delete { key: &'a [u8] },
}

/// The owned form of `GroupMember`, mirroring `WalOpOwned`'s existing
/// relationship to `WalOp` exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupMemberOwned {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

impl GroupMemberOwned {
    pub fn as_group_member(&self) -> GroupMember<'_> {
        match self {
            GroupMemberOwned::Put { key, value } => GroupMember::Put { key, value },
            GroupMemberOwned::Delete { key } => GroupMember::Delete { key },
        }
    }
}

/// One WAL operation, borrowing its key/value bytes from the caller —
/// WAL Spec §5.
///
/// No longer `Copy` as of the `Group` variant (`RELATIONAL ADR
/// AMENDMENT 001` AA.1/AA.2): `Group`'s member list is a `Vec`, which
/// cannot be `Copy`. Every existing call site constructs and consumes a
/// `WalOp` once, inline (`encode_wal_frame(seq, op, ...)`,
/// `estimate_frame_len(&op)`, `committer.append(entry.op.as_wal_op())`)
/// — none relies on implicit copying, so `Clone` (retained) is
/// sufficient everywhere `Copy` previously was.
#[derive(Debug, Clone)]
pub enum WalOp<'a> {
    Put {
        key: &'a [u8],
        value: &'a [u8],
    },
    Delete {
        key: &'a [u8],
    },
    CheckpointMarker {
        flushed_through_seq: u64,
    },
    /// `RELATIONAL ADR AMENDMENT 001` AA.1/AA.2: N operations, one
    /// shared `seq` (assigned once, at the frame level, exactly like
    /// every other `WalOp` — this variant carries no `seq` of its own).
    /// An owned `Vec` (not a borrowed slice): `WalOpOwned::as_wal_op`
    /// must build this list fresh from `GroupMemberOwned`'s own
    /// buffers, and a borrowed slice cannot outlive that construction —
    /// see `as_wal_op`'s `Group` arm.
    Group {
        members: Vec<GroupMember<'a>>,
    },
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
    Group(Vec<GroupMemberOwned>),
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
            WalOpOwned::Group(members) => WalOp::Group {
                members: members
                    .iter()
                    .map(GroupMemberOwned::as_group_member)
                    .collect(),
            },
        }
    }
}

/// Encodes one full WAL frame for `seq`/`op`, per WAL Spec §2.3/§2.4.
/// Takes `op` by value to mirror the `Wal::append(&mut self, op: WalOp)`
/// trait signature exactly, per WAL Spec §5 — moved, not copied, since
/// `WalOp` is no longer `Copy` as of the `Group` variant
/// (`RELATIONAL ADR AMENDMENT 001` AA.1).
///
/// Group 2.3: the op-body encoder is fallible and its errors propagate
/// straight out of `format::encode_frame` — an oversized key/value is
/// rejected at the exact point it's discovered (`write_len_prefixed`'s own
/// per-field check), never by emitting a deliberately-invalid on-disk
/// length and hoping a *different*, later check catches it before the
/// frame is written anywhere. `Group`'s member count is bound-checked the
/// same way (`u32::try_from`) before anything is written.
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
        WalOp::Group { members } => {
            // `RELATIONAL ADR AMENDMENT 001` AA.1/AA.2: one shared `seq`
            // (assigned once, at the frame level, above) covers every
            // member — `member_count` here is purely a decode-time
            // iteration bound, not a second sequence source.
            let member_count =
                u32::try_from(members.len()).map_err(|_| EngineError::CapacityExceeded {
                    requested: members.len() as u64,
                    max: u32::MAX as u64,
                })?;
            out.extend_from_slice(&member_count.to_le_bytes());
            for member in &members {
                match member {
                    GroupMember::Put { key, value } => {
                        out.push(OP_PUT);
                        write_len_prefixed(out, key, max_record_len)?;
                        write_len_prefixed(out, value, max_record_len)?;
                    }
                    GroupMember::Delete { key } => {
                        out.push(OP_DELETE);
                        write_len_prefixed(out, key, max_record_len)?;
                    }
                }
            }
            Ok(OP_GROUP)
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
        OP_GROUP => {
            // `RELATIONAL ADR AMENDMENT 001` AA.2's load-bearing
            // anti-DoS rule: `member_count` is an as-yet-unvalidated
            // `u32` read directly from on-disk bytes — grow `members`
            // incrementally via `.push()`, **never**
            // `Vec::with_capacity(member_count as usize)`. `op_body`
            // (and therefore `rest`) is already bounded by the frame's
            // own pre-validated `length` (<= `max_record_len`,
            // enforced by `wal::recovery::walk_segment` before this
            // function is ever called), so a bogus huge `member_count`
            // simply exhausts `rest` and returns `Corruption` after at
            // most a few real iterations — never more memory touched
            // than the already-bounded frame body itself occupies.
            let member_count = read_u32_le(op_body)?;
            let mut rest = &op_body[4..];
            let mut members = Vec::new();
            for _ in 0..member_count {
                if rest.is_empty() {
                    return Err(EngineError::Corruption {
                        detail: format!(
                            "GROUP frame declares {member_count} member(s) but ran out of \
                             bytes after {} decoded",
                            members.len()
                        ),
                    });
                }
                let member_tag = rest[0];
                rest = &rest[1..];
                let member = match member_tag {
                    OP_PUT => {
                        let (key, r1) = read_len_prefixed(rest)?;
                        let (value, r2) = read_len_prefixed(r1)?;
                        rest = r2;
                        GroupMemberOwned::Put {
                            key: key.to_vec(),
                            value: value.to_vec(),
                        }
                    }
                    OP_DELETE => {
                        let (key, r1) = read_len_prefixed(rest)?;
                        rest = r1;
                        GroupMemberOwned::Delete { key: key.to_vec() }
                    }
                    // A nested GROUP, a CHECKPOINT_MARKER, or any other
                    // tag inside a group member is never valid — fail
                    // closed rather than silently accepting a
                    // corrupted or maliciously crafted nested structure
                    // (AA.2: "structural nesting prevention on the
                    // encode side... plus this explicit rejection on
                    // the decode side together mean a nested group can
                    // never be produced or accepted").
                    other => {
                        return Err(EngineError::Corruption {
                            detail: format!(
                                "GROUP member has unsupported tag byte {other} (nested \
                                 GROUP/CHECKPOINT_MARKER are never valid)"
                            ),
                        });
                    }
                };
                members.push(member);
            }
            if !rest.is_empty() {
                return Err(EngineError::Corruption {
                    detail: format!("{} trailing byte(s) after GROUP op_body", rest.len()),
                });
            }
            WalOpOwned::Group(members)
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

    // --- `RELATIONAL ADR AMENDMENT 001` AA.2: Group (`write_batch`) frame tests ---

    #[test]
    fn group_round_trip_single_member() {
        let members = vec![GroupMember::Put {
            key: b"k1",
            value: b"v1",
        }];
        let op = WalOp::Group { members };
        let frame = encode_wal_frame(5, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let (seq, decoded) = decode_wal_body(frame_body(&frame)).unwrap();
        assert_eq!(seq, 5);
        assert_eq!(
            decoded,
            WalOpOwned::Group(vec![GroupMemberOwned::Put {
                key: b"k1".to_vec(),
                value: b"v1".to_vec(),
            }])
        );
    }

    #[test]
    fn group_round_trip_many_members_mixed_put_delete() {
        let members = vec![
            GroupMember::Put {
                key: b"a",
                value: b"1",
            },
            GroupMember::Delete { key: b"b" },
            GroupMember::Put {
                key: b"c",
                value: b"3",
            },
        ];
        let op = WalOp::Group { members };
        let frame = encode_wal_frame(11, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let (seq, decoded) = decode_wal_body(frame_body(&frame)).unwrap();
        assert_eq!(seq, 11);
        assert_eq!(
            decoded,
            WalOpOwned::Group(vec![
                GroupMemberOwned::Put {
                    key: b"a".to_vec(),
                    value: b"1".to_vec()
                },
                GroupMemberOwned::Delete { key: b"b".to_vec() },
                GroupMemberOwned::Put {
                    key: b"c".to_vec(),
                    value: b"3".to_vec()
                },
            ])
        );
    }

    #[test]
    fn group_round_trip_at_ten_thousand_members() {
        let owned: Vec<(Vec<u8>, Vec<u8>)> = (0..10_000u32)
            .map(|i| {
                (
                    format!("k{i:05}").into_bytes(),
                    format!("v{i}").into_bytes(),
                )
            })
            .collect();
        let members: Vec<GroupMember<'_>> = owned
            .iter()
            .map(|(k, v)| GroupMember::Put { key: k, value: v })
            .collect();
        let op = WalOp::Group { members };
        let frame = encode_wal_frame(1, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let (seq, decoded) = decode_wal_body(frame_body(&frame)).unwrap();
        assert_eq!(seq, 1);
        match decoded {
            WalOpOwned::Group(members) => assert_eq!(members.len(), 10_000),
            other => panic!("expected Group, got {other:?}"),
        }
    }

    #[test]
    fn group_empty_members_round_trips_at_the_wal_layer() {
        // The WAL layer itself imposes no non-empty rule — `LsmEngine::
        // write_batch` rejects an empty batch before ever constructing a
        // `Group` (AA.1), so this is purely a format-level round-trip
        // check, not an endorsement of empty batches as a live code path.
        let op = WalOp::Group { members: vec![] };
        let frame = encode_wal_frame(1, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let (seq, decoded) = decode_wal_body(frame_body(&frame)).unwrap();
        assert_eq!(seq, 1);
        assert_eq!(decoded, WalOpOwned::Group(vec![]));
    }

    #[test]
    fn group_decode_rejects_nested_group_tag() {
        // Hand-craft a GROUP frame whose single member's tag byte is
        // OP_GROUP itself (5) instead of OP_PUT/OP_DELETE.
        let mut op_body = 1u32.to_le_bytes().to_vec(); // member_count = 1
        op_body.push(OP_GROUP); // invalid member tag
        let mut body = 1u64.to_le_bytes().to_vec(); // seq
        body.push(OP_GROUP); // top-level op tag
        body.extend_from_slice(&op_body);
        let err = decode_wal_body(&body).unwrap_err();
        match err {
            EngineError::Corruption { detail } => {
                assert!(detail.contains("unsupported tag"), "detail was: {detail}")
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[test]
    fn group_decode_rejects_checkpoint_marker_tag_as_member() {
        let mut op_body = 1u32.to_le_bytes().to_vec();
        op_body.push(OP_CHECKPOINT_MARKER);
        let mut body = 1u64.to_le_bytes().to_vec();
        body.push(OP_GROUP);
        body.extend_from_slice(&op_body);
        let err = decode_wal_body(&body).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    /// The load-bearing anti-DoS test (AA.2): a `member_count` far larger
    /// than the frame's actual remaining bytes could ever hold must fail
    /// fast with `Corruption`, never attempt a huge allocation, and never
    /// hang. Bounded by this test's own timeout (the default `cargo test`
    /// per-test behavior) — if this regresses to `Vec::with_capacity
    /// (member_count as usize)`, this test either aborts the process
    /// (allocation failure) or times out, not merely fails an assertion.
    #[test]
    fn group_decode_rejects_oversized_member_count_gracefully() {
        let mut op_body = u32::MAX.to_le_bytes().to_vec(); // declares ~4 billion members
                                                           // One genuinely well-formed member (tag=DELETE, key="a") — the
                                                           // loop must decode it, then discover `rest` is truly exhausted
                                                           // while `member_count` still claims ~4 billion remain, and fail
                                                           // via the *first* check in the next iteration
                                                           // (`rest.is_empty()`) rather than misreading leftover bytes as a
                                                           // bogus tag.
        op_body.push(OP_DELETE);
        op_body.extend_from_slice(&1u32.to_le_bytes());
        op_body.push(b'a');
        let mut body = 1u64.to_le_bytes().to_vec();
        body.push(OP_GROUP);
        body.extend_from_slice(&op_body);
        let err = decode_wal_body(&body).unwrap_err();
        match err {
            EngineError::Corruption { detail } => {
                assert!(detail.contains("ran out of"), "detail was: {detail}")
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    /// A sibling of the oversized-member-count test above, covering the
    /// *other* safe failure shape: an oversized `member_count` followed
    /// by leftover bytes that don't happen to look like exhausted input
    /// but also don't decode as a valid member tag. Either shape is an
    /// acceptable, safe `Corruption` outcome (AA.2) — this test just
    /// confirms the specific "bad tag byte" path is reached (not a
    /// crash, not a hang, not a large allocation) when the leftover
    /// bytes happen to start with a non-PUT/DELETE byte.
    #[test]
    fn group_decode_rejects_oversized_member_count_with_leftover_garbage() {
        let mut op_body = u32::MAX.to_le_bytes().to_vec();
        op_body.extend_from_slice(b"only a few real bytes follow");
        let mut body = 1u64.to_le_bytes().to_vec();
        body.push(OP_GROUP);
        body.extend_from_slice(&op_body);
        let err = decode_wal_body(&body).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn group_decode_rejects_trailing_bytes_after_members() {
        let members = vec![GroupMember::Delete { key: b"a" }];
        let op = WalOp::Group { members };
        let frame = encode_wal_frame(1, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();
        let mut body = frame_body(&frame).to_vec();
        body.push(0xFF);
        let err = decode_wal_body(&body).unwrap_err();
        match err {
            EngineError::Corruption { detail } => {
                assert!(detail.contains("trailing byte"), "detail was: {detail}")
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[test]
    fn group_encode_rejects_oversized_member_key_with_capacity_exceeded() {
        let big_key = vec![0u8; 100];
        let members = vec![GroupMember::Put {
            key: &big_key,
            value: b"v",
        }];
        let op = WalOp::Group { members };
        let err = encode_wal_frame(1, op, 16).unwrap_err();
        assert!(matches!(err, EngineError::CapacityExceeded { .. }));
    }

    /// Mirrors `wire_format_is_byte_for_byte_unchanged`'s existing
    /// discipline, applied to the new `Group` frame shape: hardcodes the
    /// exact expected bytes so a byte-shifting regression in the encoder
    /// fails this test even if the higher-level round-trip tests above
    /// still happen to pass.
    #[test]
    fn group_wire_format_is_exactly_count_then_tag_and_body_per_member() {
        let members = vec![
            GroupMember::Put {
                key: b"k1",
                value: b"v1",
            },
            GroupMember::Delete { key: b"k2" },
        ];
        let op = WalOp::Group { members };
        let frame = encode_wal_frame(7, op, format::DEFAULT_MAX_RECORD_LEN).unwrap();

        let mut expected_body = Vec::new();
        expected_body.extend_from_slice(&7u64.to_le_bytes()); // seq
        expected_body.push(OP_GROUP); // top-level op tag
        expected_body.extend_from_slice(&2u32.to_le_bytes()); // member_count
        expected_body.push(OP_PUT); // member 1 tag
        expected_body.extend_from_slice(&2u32.to_le_bytes()); // key_len
        expected_body.extend_from_slice(b"k1");
        expected_body.extend_from_slice(&2u32.to_le_bytes()); // val_len
        expected_body.extend_from_slice(b"v1");
        expected_body.push(OP_DELETE); // member 2 tag
        expected_body.extend_from_slice(&2u32.to_le_bytes()); // key_len
        expected_body.extend_from_slice(b"k2");

        let expected_length_field = expected_body.len() as u32;
        let expected_crc = crc32c::crc32c(&expected_body);
        let mut expected_frame = Vec::new();
        expected_frame.extend_from_slice(&expected_length_field.to_le_bytes());
        expected_frame.extend_from_slice(&expected_crc.to_le_bytes());
        expected_frame.extend_from_slice(&expected_body);

        assert_eq!(&frame[..], &expected_frame[..]);
    }
}
