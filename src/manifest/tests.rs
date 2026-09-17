use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::error::EngineError;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "rubixdb_manifest_test_{tag}_{}_{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn replay_readonly_on_absent_manifest_is_empty_not_an_error() {
    let dir = temp_dir("absent");
    let result = replay_readonly(&dir).unwrap();
    assert!(result.state.live_sstables.is_empty());
    assert_eq!(result.state.checkpoint_seq(), 0);
    cleanup(&dir);
}

#[test]
fn open_creates_the_file_and_append_sync_round_trips_through_replay() {
    let dir = temp_dir("append-replay");
    {
        let (mut manifest, initial) = Manifest::open_after_exclusive_lock(&dir).unwrap();
        assert!(initial.state.live_sstables.is_empty());
        manifest
            .append_sync(ManifestEdit::AddSstable {
                id: 1,
                min_seq: 1,
                max_seq: 100,
                file_size: 4096,
            })
            .unwrap();
        manifest
            .append_sync(ManifestEdit::SetCheckpoint {
                flushed_through_seq: 100,
                wal_segment_id: 1,
                wal_offset: 24,
            })
            .unwrap();
    }

    let replayed = replay_readonly(&dir).unwrap();
    assert_eq!(replayed.state.live_sstables.len(), 1);
    assert_eq!(replayed.state.checkpoint_seq(), 100);
    cleanup(&dir);
}

#[test]
fn reopen_after_exclusive_lock_truncates_a_torn_tail_and_can_still_append() {
    let dir = temp_dir("torn-reopen");
    {
        let (mut manifest, _) = Manifest::open_after_exclusive_lock(&dir).unwrap();
        manifest
            .append_sync(ManifestEdit::AddSstable {
                id: 1,
                min_seq: 1,
                max_seq: 10,
                file_size: 10,
            })
            .unwrap();
    }
    // Simulate a torn write: append a few garbage bytes after the last
    // valid frame.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("MANIFEST"))
            .unwrap();
        f.write_all(&[1, 2, 3]).unwrap();
    }

    let (mut manifest, replay_result) = Manifest::open_after_exclusive_lock(&dir).unwrap();
    assert!(replay_result.truncated);
    assert_eq!(replay_result.state.live_sstables.len(), 1);
    // The file must now be physically truncated -- appending a new,
    // valid edit afterward must not leave the old garbage bytes
    // in-between, which would corrupt any future replay.
    manifest
        .append_sync(ManifestEdit::RemoveSstable { id: 1 })
        .unwrap();
    drop(manifest);

    let final_replay = replay_readonly(&dir).unwrap();
    assert!(!final_replay.truncated);
    assert!(final_replay.state.live_sstables.is_empty());
    assert!(final_replay.state.ever_added.contains(&1));
    cleanup(&dir);
}

#[test]
fn non_tail_corruption_fails_closed_on_open() {
    let dir = temp_dir("corrupt-open");
    {
        let (mut manifest, _) = Manifest::open_after_exclusive_lock(&dir).unwrap();
        manifest
            .append_sync(ManifestEdit::RemoveSstable { id: 5 }) // never added -- corruption
            .unwrap();
    }
    let err = Manifest::open_after_exclusive_lock(&dir).unwrap_err();
    assert!(matches!(err, EngineError::Corruption { .. }));
    cleanup(&dir);
}

#[test]
fn multiple_flushes_accumulate_correctly() {
    let dir = temp_dir("multi-flush");
    {
        let (mut manifest, _) = Manifest::open_after_exclusive_lock(&dir).unwrap();
        for id in 1..=5u64 {
            manifest
                .append_sync(ManifestEdit::AddSstable {
                    id,
                    min_seq: id * 10,
                    max_seq: id * 10 + 9,
                    file_size: 1000,
                })
                .unwrap();
            manifest
                .append_sync(ManifestEdit::SetCheckpoint {
                    flushed_through_seq: id * 10 + 9,
                    wal_segment_id: 1,
                    wal_offset: id * 100,
                })
                .unwrap();
        }
    }
    let replayed = replay_readonly(&dir).unwrap();
    assert_eq!(replayed.state.live_sstables.len(), 5);
    assert_eq!(replayed.state.checkpoint_seq(), 59);
    cleanup(&dir);
}

// ---------------------------------------------------------------------
// Property test: real Manifest (append_sync + replay) vs. an
// independent reference model, over randomized, semantically-legal
// operation sequences (operating brief: "edit replay equivalence,"
// "repeated recovery," "do not use the implementation itself as its
// own oracle"). The reference model below never calls `ManifestState`
// -- it is a separate, from-scratch reimplementation of the same
// three rules.
// ---------------------------------------------------------------------

mod property {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

    #[derive(Debug, Clone, Copy)]
    enum Op {
        AddNew,
        RemoveLive(u32), // index into the current live set, modulo its length
        AdvanceCheckpoint(u16),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => Just(Op::AddNew),
            2 => any::<u32>().prop_map(Op::RemoveLive),
            2 => any::<u16>().prop_map(Op::AdvanceCheckpoint),
        ]
    }

    /// A from-scratch reference model -- deliberately not built on top
    /// of `ManifestState`, per the operating brief's own "do not use
    /// the implementation as its own oracle" instruction.
    #[derive(Debug, Default)]
    struct ReferenceModel {
        next_id: u64,
        live: HashSet<u64>,
        checkpoint: u64,
    }

    fn apply_ops(ops: &[Op]) -> (ReferenceModel, Vec<ManifestEdit>) {
        let mut model = ReferenceModel::default();
        let mut edits = Vec::new();
        let mut live_order: Vec<u64> = Vec::new();
        for op in ops {
            match *op {
                Op::AddNew => {
                    model.next_id += 1;
                    let id = model.next_id;
                    model.live.insert(id);
                    live_order.push(id);
                    edits.push(ManifestEdit::AddSstable {
                        id,
                        min_seq: id,
                        max_seq: id,
                        file_size: id * 100,
                    });
                }
                Op::RemoveLive(idx) => {
                    if !live_order.is_empty() {
                        let id = live_order[idx as usize % live_order.len()];
                        if model.live.remove(&id) {
                            live_order.retain(|&x| x != id);
                            edits.push(ManifestEdit::RemoveSstable { id });
                        }
                    }
                }
                Op::AdvanceCheckpoint(delta) => {
                    model.checkpoint += delta as u64;
                    edits.push(ManifestEdit::SetCheckpoint {
                        flushed_through_seq: model.checkpoint,
                        wal_segment_id: 1,
                        wal_offset: model.checkpoint,
                    });
                }
            }
        }
        (model, edits)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn manifest_replay_matches_independent_reference_model(ops in proptest::collection::vec(op_strategy(), 1..100)) {
            let (model, edits) = apply_ops(&ops);
            let dir = temp_dir("property-replay");
            {
                let (mut manifest, _) = Manifest::open_after_exclusive_lock(&dir).unwrap();
                for edit in &edits {
                    manifest.append_sync(*edit).unwrap();
                }
            }

            let replayed = replay_readonly(&dir).unwrap();
            let actual_live: HashSet<u64> = replayed.state.live_sstables.keys().copied().collect();
            prop_assert_eq!(actual_live, model.live.clone());
            prop_assert_eq!(replayed.state.checkpoint_seq(), model.checkpoint);

            // Repeated recovery: replaying the same file twice must
            // produce the same logical state both times.
            let replayed_again = replay_readonly(&dir).unwrap();
            let actual_live_again: HashSet<u64> =
                replayed_again.state.live_sstables.keys().copied().collect();
            prop_assert_eq!(actual_live_again, model.live);
            prop_assert_eq!(replayed_again.state.checkpoint_seq(), model.checkpoint);

            cleanup(&dir);
        }
    }
}
