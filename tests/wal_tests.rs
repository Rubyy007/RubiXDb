//! Integration tests for the WAL against real files on disk, exercising
//! the public `FileWal`/`inspect` API exactly as an external caller would.
//! Covers the WAL Spec §11 checklist items that specifically need real
//! multi-segment directories and real file truncation/corruption (tests
//! #1, #6, #7, #8, #9, #10, #12, #13, #15). Pure byte-level classification
//! logic (torn-vs-corrupt at arbitrary offsets, tests #2–5) and the
//! fsync-failure test (#11) are unit-tested inside the crate instead,
//! where the private `walk_segment`/`FaultInjectingIo` machinery is
//! reachable — see `src/wal/recovery.rs` and `src/wal/testing.rs`. The
//! ≥1,000-run fuzz test (#14) is `src/wal/fuzz_tests.rs`, run in-memory
//! for speed.
//!
//! Temp directories: this project has no `tempfile` dependency (Tier 2 —
//! not worth adding for test-only scaffolding), so each test gets a
//! unique directory under `std::env::temp_dir()` via a process-wide atomic
//! counter plus the current time, cleaned up best-effort on drop.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rubixdb::error::EngineError;
use rubixdb::wal::{inspect, FileWal, Wal, WalConfig, WalOp, WalOpOwned};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(test_name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("rubixdb_wal_test_{test_name}_{nanos}_{n}"));
        fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn small_config(max_segment_size: u64) -> WalConfig {
    WalConfig {
        max_segment_size,
        ..WalConfig::default()
    }
}

fn wal_segment_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("wal")
}

fn list_segment_paths(data_dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(wal_segment_dir(data_dir))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "log").unwrap_or(false))
        .collect();
    paths.sort();
    paths
}

/// Test #1 + #2 (round trip; delete tombstone is not silently absent).
#[test]
fn round_trip_multiple_records_and_op_types() {
    let dir = TempDir::new("round_trip");
    let (mut wal, first_result) =
        FileWal::open_for_recovery(dir.path(), WalConfig::default()).unwrap();
    assert!(first_result.records.is_empty());
    assert_eq!(wal.next_seq(), 1);

    wal.append_sync(WalOp::Put {
        key: b"k1",
        value: b"v1",
    })
    .unwrap();
    wal.append_sync(WalOp::Put {
        key: b"k2",
        value: b"",
    })
    .unwrap();
    wal.append_sync(WalOp::Delete { key: b"k1" }).unwrap();
    wal.append_sync(WalOp::CheckpointMarker {
        flushed_through_seq: 2,
    })
    .unwrap();
    drop(wal);

    let (wal2, result) = FileWal::open_for_recovery(dir.path(), WalConfig::default()).unwrap();
    assert!(!result.truncated);
    assert!(result.corrupted_segments.is_empty());
    assert_eq!(wal2.next_seq(), 5);
    assert_eq!(
        result.records,
        vec![
            (
                1,
                WalOpOwned::Put {
                    key: b"k1".to_vec(),
                    value: b"v1".to_vec()
                }
            ),
            (
                2,
                WalOpOwned::Put {
                    key: b"k2".to_vec(),
                    value: Vec::new()
                }
            ),
            (
                3,
                WalOpOwned::Delete {
                    key: b"k1".to_vec()
                }
            ),
            (
                4,
                WalOpOwned::CheckpointMarker {
                    flushed_through_seq: 2
                }
            ),
        ]
    );
}

/// Test #15.
#[test]
fn empty_wal_recovers_cleanly() {
    let dir = TempDir::new("empty");
    let (wal, result) = FileWal::open_for_recovery(dir.path(), WalConfig::default()).unwrap();
    assert!(result.records.is_empty());
    assert!(!result.truncated);
    assert!(result.corrupted_segments.is_empty());
    assert_eq!(wal.next_seq(), 1);
}

/// Test #13.
#[test]
fn sequence_number_resumes_across_reopen() {
    let dir = TempDir::new("seq_resume");
    let (mut wal, _) = FileWal::open_for_recovery(dir.path(), WalConfig::default()).unwrap();
    for i in 0..5u32 {
        wal.append_sync(WalOp::Put {
            key: format!("k{i}").as_bytes(),
            value: b"v",
        })
        .unwrap();
    }
    drop(wal);

    let (mut wal2, result) = FileWal::open_for_recovery(dir.path(), WalConfig::default()).unwrap();
    assert_eq!(result.records.len(), 5);
    assert_eq!(wal2.next_seq(), 6);
    let pos = wal2.append_sync(WalOp::Delete { key: b"kx" }).unwrap();
    assert_eq!(pos.seq, 6);
}

/// Test #6.
#[test]
fn multi_segment_replay_reconstructs_full_order() {
    let dir = TempDir::new("multi_segment");
    // Small enough that a handful of ~40-byte records force several
    // rotations.
    let (mut wal, _) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
    for i in 0..40u32 {
        wal.append_sync(WalOp::Put {
            key: format!("key-{i:03}").as_bytes(),
            value: b"value-bytes",
        })
        .unwrap();
    }
    let last_segment_id = wal.current_segment_id();
    assert!(
        last_segment_id >= 5,
        "expected several rotations, got segment id {last_segment_id}"
    );
    drop(wal);

    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
    assert!(!result.truncated);
    assert!(result.corrupted_segments.is_empty());
    assert_eq!(result.records.len(), 40);
    for (i, (seq, _)) in result.records.iter().enumerate() {
        assert_eq!(*seq, (i as u64) + 1);
    }
}

/// Test #7.
#[test]
fn torn_tail_only_in_last_segment_is_truncated_not_flagged_corrupt() {
    let dir = TempDir::new("torn_tail_last_segment");
    {
        let (mut wal, _) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
        for i in 0..30u32 {
            wal.append_sync(WalOp::Put {
                key: format!("key-{i:03}").as_bytes(),
                value: b"value-bytes",
            })
            .unwrap();
        }
    } // wal (and its file handles) dropped here

    let segments = list_segment_paths(dir.path());
    assert!(segments.len() >= 3, "need multiple segments for this test");
    let last_segment = segments.last().unwrap();
    let len_before = fs::metadata(last_segment).unwrap().len();
    assert!(len_before > 4, "segment too small to meaningfully truncate");
    let f = OpenOptions::new().write(true).open(last_segment).unwrap();
    f.set_len(len_before - 3).unwrap();

    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
    assert!(result.truncated);
    assert!(result.corrupted_segments.is_empty());
    assert!(result.records.len() < 30);

    // Recovery must have physically truncated the segment (§6.4) — a
    // second open must not re-discover the same torn tail as an error.
    let (_, result2) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
    assert!(result2.corrupted_segments.is_empty());
    assert_eq!(result2.records.len(), result.records.len());
}

/// Test #8, **superseded by Group 3.1**: WAL Spec §6.2 originally allowed
/// recovery to keep scanning past a corrupted non-last segment so later,
/// intact segments could still contribute records — this test used to
/// assert exactly that (see git history / `PROGRESS.md` for the prior
/// version, named `..._does_not_stop_the_scan`). Group 3.1 deliberately
/// amended that behavior to fail-closed: stop at the *first* corrupted
/// segment and trust nothing at or after it (see `wal::mod`'s "#
/// Durability" section for the full rationale). This is a real, flagged
/// behavior change, not a silent edit — the old assertions here are now
/// impossible to satisfy simultaneously with Group 3.1's requirement, so
/// this test was updated to assert the new contract instead of removed.
#[test]
fn corrupted_header_on_non_last_segment_stops_the_scan() {
    let dir = TempDir::new("corrupted_header_non_last");
    {
        let (mut wal, _) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
        for i in 0..30u32 {
            wal.append_sync(WalOp::Put {
                key: format!("key-{i:03}").as_bytes(),
                value: b"value-bytes",
            })
            .unwrap();
        }
    }

    let segments = list_segment_paths(dir.path());
    assert!(segments.len() >= 3);
    let first_segment = &segments[0];
    let mut f = OpenOptions::new().write(true).open(first_segment).unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(b"XXXXXXXX").unwrap(); // corrupt the magic

    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
    assert_eq!(result.corrupted_segments, vec![1]);
    assert!(
        result.records.is_empty(),
        "segment 1 (the first, and here the corrupted one) precedes everything else, \
         so nothing after it is trusted or even scanned"
    );
}

/// Group 3.1 regression test, the complement of the above: corruption on
/// a *later* non-first segment must still let every earlier segment's
/// records through, and must still stop before anything after it.
#[test]
fn corrupted_non_first_non_last_segment_keeps_earlier_records_only() {
    let dir = TempDir::new("corrupted_middle_segment");
    {
        let (mut wal, _) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
        for i in 0..30u32 {
            wal.append_sync(WalOp::Put {
                key: format!("key-{i:03}").as_bytes(),
                value: b"value-bytes",
            })
            .unwrap();
        }
    }

    let segments = list_segment_paths(dir.path());
    assert!(
        segments.len() >= 3,
        "need at least 3 segments for this test"
    );
    let middle_segment = &segments[1];
    let mut f = OpenOptions::new().write(true).open(middle_segment).unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(b"XXXXXXXX").unwrap(); // corrupt the magic

    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
    assert_eq!(result.corrupted_segments.len(), 1);
    assert!(
        !result.records.is_empty(),
        "the first segment's records precede the corruption and must still be trusted"
    );
    assert!(
        result.records.len() < 30,
        "nothing from the corrupted segment or anything after it is included"
    );
}

/// Test #9.
#[test]
fn max_record_len_is_enforced_before_any_write() {
    let dir = TempDir::new("max_record_len");
    let config = WalConfig {
        max_record_len: 16,
        ..WalConfig::default()
    };
    let (mut wal, _) = FileWal::open_for_recovery(dir.path(), config).unwrap();

    let segments = list_segment_paths(dir.path());
    let len_before = fs::metadata(&segments[0]).unwrap().len();

    let err = wal
        .append(WalOp::Put {
            key: b"this key alone is already longer than 16 bytes",
            value: b"v",
        })
        .unwrap_err();
    assert!(matches!(err, EngineError::CapacityExceeded { .. }));

    let len_after = fs::metadata(&segments[0]).unwrap().len();
    assert_eq!(
        len_before, len_after,
        "a rejected append must not write any bytes"
    );
}

/// Test #10.
#[test]
fn purge_before_only_removes_fully_superseded_sealed_segments() {
    let dir = TempDir::new("purge_before");
    let (mut wal, _) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
    for i in 0..30u32 {
        wal.append_sync(WalOp::Put {
            key: format!("key-{i:03}").as_bytes(),
            value: b"value-bytes",
        })
        .unwrap();
    }
    let active_id = wal.current_segment_id();
    let segments_before = list_segment_paths(dir.path());
    assert!(segments_before.len() >= 3);

    // A watermark of 1 should remove nothing (every segment has a record
    // with seq >= 1).
    let removed_none = wal.purge_before(1).unwrap();
    assert!(removed_none.is_empty());

    // A very high watermark should remove every *sealed* segment but
    // never the active one.
    let removed = wal.purge_before(u64::MAX).unwrap();
    assert!(!removed.is_empty());
    assert!(!removed.contains(&active_id));

    let segments_after = list_segment_paths(dir.path());
    assert_eq!(segments_after.len(), segments_before.len() - removed.len());
    assert!(segments_after
        .iter()
        .any(|p| p.to_string_lossy().contains(&format!("{active_id:020}"))));
}

/// Test #12.
#[test]
fn inspect_never_mutates_the_directory() {
    let dir = TempDir::new("inspect_side_effect_free");
    {
        let (mut wal, _) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
        for i in 0..10u32 {
            wal.append_sync(WalOp::Put {
                key: format!("key-{i}").as_bytes(),
                value: b"v",
            })
            .unwrap();
        }
    }
    let segments = list_segment_paths(dir.path());
    let last = segments.last().unwrap();
    let len_before = fs::metadata(last).unwrap().len();
    let f = OpenOptions::new().write(true).open(last).unwrap();
    f.set_len(len_before - 2).unwrap();

    let snapshot_before: Vec<(PathBuf, Vec<u8>)> = list_segment_paths(dir.path())
        .into_iter()
        .map(|p| {
            let mut bytes = Vec::new();
            File::open(&p).unwrap().read_to_end(&mut bytes).unwrap();
            (p, bytes)
        })
        .collect();

    let result = inspect(dir.path(), &small_config(128)).unwrap();
    assert!(result.truncated);

    let snapshot_after: Vec<(PathBuf, Vec<u8>)> = list_segment_paths(dir.path())
        .into_iter()
        .map(|p| {
            let mut bytes = Vec::new();
            File::open(&p).unwrap().read_to_end(&mut bytes).unwrap();
            (p, bytes)
        })
        .collect();

    assert_eq!(
        snapshot_before, snapshot_after,
        "inspect() must not alter any bytes on disk"
    );
}

/// Group 4.2 regression test, as specified: "mark the WAL dir read-only
/// (chmod 0o555 on Unix) and assert inspect succeeds while
/// open_for_recovery fails with PermissionDenied." `chmod` permission bits
/// are a Unix concept — Windows' read-only file attribute does not
/// prevent creating/writing files inside a directory the way Unix mode
/// bits do, so there is no equivalent test to write for Windows here;
/// this is `#[cfg(unix)]`-only, matching the spec's own framing.
#[cfg(unix)]
#[test]
fn inspect_works_on_a_read_only_directory_but_open_for_recovery_does_not() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new("read_only_dir");
    {
        let (mut wal, _) = FileWal::open_for_recovery(dir.path(), WalConfig::default()).unwrap();
        wal.append_sync(WalOp::Put {
            key: b"k",
            value: b"v",
        })
        .unwrap();
    }

    let wal_dir = wal_segment_dir(dir.path());
    let original_perms = fs::metadata(&wal_dir).unwrap().permissions();
    fs::set_permissions(&wal_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let inspect_result = inspect(dir.path(), &WalConfig::default());
    let open_result = FileWal::open_for_recovery(dir.path(), WalConfig::default());

    // Restore write permission before the `TempDir` guard tries to
    // recursively remove the directory on drop, regardless of what the
    // assertions below find.
    fs::set_permissions(&wal_dir, original_perms).unwrap();

    let result = inspect_result.unwrap();
    assert_eq!(result.records.len(), 1);

    match open_result {
        Err(EngineError::Io(e)) => {
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
        }
        other => panic!("expected a PermissionDenied Io error, got {other:?}"),
    }
}

const ENV_LOCK_PROBE_DIR: &str = "RUBIXDB_LOCK_PROBE_DIR";

/// Regression test for the missing-file-locking gap: a genuinely separate
/// **process** (not just a second in-process call) trying to open the same
/// WAL directory while this process holds it open must be rejected, and
/// must succeed once this process's `FileWal` is dropped. Uses the same
/// "re-exec this test binary, select one test by exact name" trick as
/// `tests/crash_consistency.rs`.
#[test]
fn cross_process_lock_prevents_concurrent_writers() {
    let dir = TempDir::new("cross_process_lock");
    let exe = std::env::current_exe().unwrap();

    let (wal, _) = FileWal::open_for_recovery(dir.path(), WalConfig::default()).unwrap();

    let status_while_held = Command::new(&exe)
        .arg("lock_probe_child")
        .arg("--exact")
        .arg("--nocapture")
        .env(ENV_LOCK_PROBE_DIR, dir.path())
        .status()
        .expect("failed to spawn child probe process");
    assert!(
        !status_while_held.success(),
        "a separate process must not be able to open the WAL directory while this \
         process holds it"
    );

    drop(wal);

    let status_after_drop = Command::new(&exe)
        .arg("lock_probe_child")
        .arg("--exact")
        .arg("--nocapture")
        .env(ENV_LOCK_PROBE_DIR, dir.path())
        .status()
        .expect("failed to spawn child probe process");
    assert!(
        status_after_drop.success(),
        "a separate process must be able to open the WAL directory once this \
         process's FileWal is dropped"
    );
}

/// The child-process side of the test above. An ordinary `#[test]` so it
/// can be selected by exact name from a spawned child; under a normal,
/// un-parented `cargo test` sweep the environment variable is absent and
/// this immediately returns, passing trivially.
#[test]
fn lock_probe_child() {
    let Ok(dir) = std::env::var(ENV_LOCK_PROBE_DIR) else {
        return;
    };
    match FileWal::open_for_recovery(Path::new(&dir), WalConfig::default()) {
        Ok(_) => std::process::exit(0),
        Err(_) => std::process::exit(1),
    }
}
