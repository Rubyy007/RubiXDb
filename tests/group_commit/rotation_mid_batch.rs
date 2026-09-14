//! M1.5: `rotate()` called while a batch is pending (followers already
//! registered, waiting on the leader's `fsync`) must not disrupt
//! durability: no waiter hangs, every acknowledged `seq` is durable after
//! recovery, and no acknowledged `seq` is ever falsely reported durable.
//!
//! Per `PROCESS.md` §1.5, mid-batch rotation needs no special-case logic
//! in `GroupCommitter` itself — `seq` is WAL-wide (not segment-scoped) and
//! a leader's sync target is snapshotted atomically with respect to
//! `rotate()` (both require the same `wal` lock). This test exercises that
//! claim under real concurrent load rather than merely asserting it: 100
//! writer threads hammer `append_durable`-shaped calls while a separate
//! thread calls `rotate()` at an unpredictable point during the run.

use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rubixdb::wal::{FileWal, GroupCommitter, Wal, WalConfig, WalOp};

use crate::support;

const WRITERS: usize = 100;
const PER_WRITER: usize = 20;
const NO_HANG_BOUND: Duration = Duration::from_secs(15);

#[test]
fn rotation_mid_batch() {
    let dir = support::temp_dir("m1_5");
    let (wal, _) = FileWal::open_for_recovery(&dir, support::group_commit_config())
        .expect("opening a fresh WAL must succeed");
    let committer =
        Arc::new(GroupCommitter::new(wal).expect("wal was opened with SyncMode::GroupCommit"));

    // A small, unpredictable delay before rotating, derived from the
    // current time rather than a fixed sleep, so the exact point rotate()
    // lands at relative to the 100 concurrent writers' batches varies
    // from run to run rather than being pinned to one fixed offset.
    let rotator_committer = Arc::clone(&committer);
    let rotate_handle = thread::spawn(move || {
        let jitter_micros = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 500) as u64;
        thread::sleep(Duration::from_micros(jitter_micros));
        rotator_committer
            .rotate()
            .expect("rotate must succeed against a healthy temp directory");
    });

    let (tx, rx) = mpsc::channel();
    let handles: Vec<_> = (0..WRITERS)
        .map(|t| {
            let committer = Arc::clone(&committer);
            let tx = tx.clone();
            thread::spawn(move || {
                let mut acknowledged = Vec::with_capacity(PER_WRITER);
                for i in 0..PER_WRITER {
                    let key = format!("t{t}-{i}");
                    let position = committer
                        .append(WalOp::Put {
                            key: key.as_bytes(),
                            value: b"v",
                        })
                        .expect("append itself never fails in this scenario: no fault injected");
                    support::await_durable_retrying_on_timeout(&committer, position.seq);
                    acknowledged.push(position.seq);
                }
                let _ = tx.send(acknowledged);
            })
        })
        .collect();
    drop(tx);

    rotate_handle.join().expect("rotator thread must not panic");

    let mut all_acknowledged = Vec::with_capacity(WRITERS * PER_WRITER);
    for _ in 0..WRITERS {
        // (a) no waiter hangs: bounded by NO_HANG_BOUND, not an assumption.
        let acknowledged = rx
            .recv_timeout(NO_HANG_BOUND)
            .expect("a writer thread's result must arrive within the no-hang bound");
        all_acknowledged.extend(acknowledged);
    }
    for h in handles {
        h.join().expect("writer thread must not panic");
    }

    let segments_touched = committer.current_segment_id();
    assert!(
        segments_touched > 1,
        "rotate() must actually have run and sealed at least one segment \
         (current_segment_id={segments_touched}) — otherwise this test \
         never exercised the mid-batch-rotation path it claims to"
    );

    let committer = Arc::into_inner(committer)
        .expect("no outstanding Arc clones remain after all threads joined");
    let wal = committer.into_inner();
    drop(wal);

    let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default())
        .expect("reopening after a clean shutdown must succeed");
    assert!(replay.corrupted_segments.is_empty());

    let recovered: HashSet<u64> = replay.records.iter().map(|(seq, _)| *seq).collect();

    // (b) every acknowledged S is durable after recovery.
    for seq in &all_acknowledged {
        assert!(
            recovered.contains(seq),
            "acknowledged seq {seq} is missing after recovery — durability violated across rotation"
        );
    }
    assert_eq!(
        all_acknowledged.len(),
        WRITERS * PER_WRITER,
        "every writer must have acknowledged all of its records"
    );

    // (c) no acknowledged S is reported durable when it isn't: in this
    // clean-shutdown (no fault, no real crash) scenario, the recovered set
    // must be *exactly* what was acknowledged — nothing acknowledged is
    // missing (checked above) and nothing extra/unexplained is present
    // either, since every append in this test went through
    // await_durable_retrying_on_timeout, which only returns once
    // acknowledged.
    assert_eq!(
        replay.records.len(),
        all_acknowledged.len(),
        "recovered record count must exactly match the acknowledged count \
         in this fault-free, clean-shutdown scenario"
    );
    for (i, (seq, _)) in replay.records.iter().enumerate() {
        assert_eq!(
            *seq,
            (i as u64) + 1,
            "seq must be gap-free and in order across rotation"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
