//! Tests for `FileWal::append_group` (the batched write underneath the
//! group-commit flat-combining `append`): equivalence with sequential
//! `append`, per-op encode failures, whole-run write failures, rotation
//! inside a group, and combiner panic safety.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;

use super::*;

static N: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rubixdb_group_append_{tag}_{nanos}_{n}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn cfg(max_segment_size: u64, max_record_len: usize) -> WalConfig {
    WalConfig {
        max_segment_size,
        max_record_len,
        ..WalConfig::default()
    }
}

#[derive(Debug, Clone)]
enum Spec {
    Put(usize, usize),
    Del(usize),
    Group(usize),
}

fn materialize(spec: &Spec, i: usize) -> WalOpOwned {
    match spec {
        Spec::Put(k, v) => WalOpOwned::Put {
            key: format!("k{i}-{}", "x".repeat(*k)).into_bytes(),
            value: vec![b'v'; *v],
        },
        Spec::Del(k) => WalOpOwned::Delete {
            key: format!("d{i}-{}", "y".repeat(*k)).into_bytes(),
        },
        Spec::Group(m) => WalOpOwned::Group(
            (0..*m)
                .map(|j| GroupMemberOwned::Put {
                    key: format!("g{i}-{j}").into_bytes(),
                    value: vec![b'g'; 3],
                })
                .collect(),
        ),
    }
}

fn spec_strategy() -> impl Strategy<Value = Spec> {
    prop_oneof![
        (0usize..40, 0usize..200).prop_map(|(k, v)| Spec::Put(k, v)),
        (0usize..40).prop_map(Spec::Del),
        (1usize..5).prop_map(Spec::Group),
    ]
}

fn replay_of(dir: &std::path::Path, c: WalConfig) -> Vec<(u64, WalOpOwned)> {
    let (_wal, replay) = FileWal::open_for_recovery(dir, c).unwrap();
    assert!(replay.corrupted_segments.is_empty());
    replay.records
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// `append_group` must be observationally identical to sequential
    /// `append`: same per-op Ok/Err pattern, same positions, same recovered
    /// records -- including when a tiny segment size forces rotations inside
    /// the group and a small `max_record_len` makes some encodes fail.
    #[test]
    fn append_group_equals_sequential_appends(
        specs in proptest::collection::vec(spec_strategy(), 1..40),
        seg in 160u64..1200,
    ) {
        let c = || cfg(seg, 150);
        let ops: Vec<WalOpOwned> = specs.iter().enumerate().map(|(i, s)| materialize(s, i)).collect();

        let dir_a = temp_dir("seq");
        let (mut wal_a, _) = FileWal::open_for_recovery(&dir_a, c()).unwrap();
        let seq_results: Vec<Result<WalPosition>> =
            ops.iter().map(|o| wal_a.append(o.as_wal_op())).collect();
        wal_a.sync().unwrap();
        drop(wal_a);

        let dir_b = temp_dir("grp");
        let (mut wal_b, _) = FileWal::open_for_recovery(&dir_b, c()).unwrap();
        let grp_results = wal_b.append_group(ops.iter().map(|o| o.as_wal_op()).collect());
        wal_b.sync().unwrap();
        drop(wal_b);

        prop_assert_eq!(seq_results.len(), grp_results.len());
        for (a, b) in seq_results.iter().zip(grp_results.iter()) {
            match (a, b) {
                (Ok(pa), Ok(pb)) => prop_assert_eq!(pa, pb),
                (Err(_), Err(_)) => {}
                _ => prop_assert!(false, "Ok/Err pattern differs: {:?} vs {:?}", a, b),
            }
        }
        prop_assert_eq!(replay_of(&dir_a, c()), replay_of(&dir_b, c()));
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }
}

#[test]
fn an_encode_failure_fails_only_that_op_and_consumes_no_seq() {
    let dir = temp_dir("enc");
    let (mut wal, _) = FileWal::open_for_recovery(&dir, cfg(1 << 20, 64)).unwrap();
    let ok = |n: u8| WalOpOwned::Put {
        key: vec![n],
        value: vec![n; 8],
    };
    let too_big = WalOpOwned::Put {
        key: vec![9],
        value: vec![0; 4096],
    };
    let r = wal.append_group(vec![
        ok(1).as_wal_op(),
        too_big.as_wal_op(),
        ok(2).as_wal_op(),
    ]);
    assert_eq!(r[0].as_ref().unwrap().seq, 1);
    assert!(r[1].is_err());
    assert_eq!(
        r[2].as_ref().unwrap().seq,
        2,
        "the failed op consumed no seq"
    );
    wal.sync().unwrap();
    drop(wal);
    let recs = replay_of(&dir, cfg(1 << 20, 64));
    assert_eq!(recs.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![1, 2]);
}

#[test]
fn a_failed_group_write_fails_every_writer_in_it_and_leaves_no_trace_or_seq_gap() {
    let dir = temp_dir("fail");
    let c = || cfg(1 << 20, 150);
    let (mut wal, _) = FileWal::open_for_recovery(&dir, c()).unwrap();
    let op = |n: u8| WalOpOwned::Put {
        key: vec![n],
        value: vec![n; 8],
    };

    let first = wal.append_group(vec![op(1).as_wal_op(), op(2).as_wal_op()]);
    assert!(first.iter().all(|r| r.is_ok()));

    wal.fail_group_write_on_nth = Some((1, std::io::ErrorKind::StorageFull));
    let failed = wal.append_group(vec![
        op(3).as_wal_op(),
        op(4).as_wal_op(),
        op(5).as_wal_op(),
    ]);
    assert!(
        failed.iter().all(|r| r.is_err()),
        "every writer in a failed batched write gets Err"
    );
    assert_eq!(
        wal.next_seq(),
        3,
        "next_seq is rewound: no caller holds Ok for the failed seqs"
    );

    // The WAL keeps working (same recoverable behavior as a failed single append),
    // and the next records reuse seqs 3.. with no gap and no trace of the failed run.
    let after = wal.append_group(vec![op(6).as_wal_op(), op(7).as_wal_op()]);
    assert_eq!(after[0].as_ref().unwrap().seq, 3);
    assert_eq!(after[1].as_ref().unwrap().seq, 4);
    wal.sync().unwrap();
    drop(wal);
    let recs = replay_of(&dir, c());
    assert_eq!(
        recs.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert!(!recs.iter().any(|(_, o)| matches!(o, WalOpOwned::Put { key, .. } if key == &vec![3u8] || key == &vec![4u8] || key == &vec![5u8])));
}

#[test]
fn rotation_inside_a_group_keeps_the_written_prefix_when_a_later_run_fails() {
    let dir = temp_dir("rot");
    // Segment so small that each op forces a rotation: every frame is its own run.
    let c = || cfg(100, 150);
    let (mut wal, _) = FileWal::open_for_recovery(&dir, c()).unwrap();
    let op = |n: u8| WalOpOwned::Put {
        key: vec![n],
        value: vec![n; 40],
    };
    // The 3rd flush_run (the 3rd run) fails.
    wal.fail_group_write_on_nth = Some((3, std::io::ErrorKind::Other));
    let owned: Vec<WalOpOwned> = (1..=5u8).map(op).collect();
    let r = wal.append_group(owned.iter().map(|o| o.as_wal_op()).collect());
    assert!(
        r[0].is_ok() && r[1].is_ok(),
        "runs before the failure keep their Ok"
    );
    assert!(
        r[2].is_err() && r[3].is_err() && r[4].is_err(),
        "the failed run and everything after it fail"
    );
    assert_eq!(wal.next_seq(), 3, "next_seq == first failed seq");
    wal.sync().unwrap();
    drop(wal);
    let recs = replay_of(&dir, c());
    assert_eq!(recs.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![1, 2]);
}
