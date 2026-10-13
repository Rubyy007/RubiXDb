//! Regression tests for the D1a decision-gate findings (docs/D1a_DECISION_GATE.md). Each is written to FAIL on the code as it
//! stands and to PASS once the corresponding PROPOSED ADR is implemented; none was written to pass today, none is ignored, and
//! none weakens an existing test. They need the fault-point registry (`--features fault-points`): F-D1a-1 uses the
//! `lsm.before_apply` seam and F-D0-3 uses `shortwrite` at the real `Manifest::append_sync` call site.
//!
//! Run: `cargo test --features fault-points --test pending_fixes -- --test-threads=1`
//!
//! Anti-tests (the over-broad-fix guards: they pass today and must keep passing) are marked `guard_`.
//!
//! All crash images here are process-level (class B): files copied from, or left behind by, a live process. They say nothing
//! about power loss.

#![cfg(feature = "fault-points")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::faults;
use rubixdb::lsm::{LsmConfig, LsmEngine, WriteOp};
use rubixdb::manifest::{self, Manifest, ManifestEdit};
use rubixdb::wal::{self, FileWal, SyncMode, Wal, WalConfig, WalOp};

/// The registry is process-global; every test serialises on this lock so an armed rule can never fire in another test.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "rbx_pending_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn product_wal() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

fn pool() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig {
        queue_capacity: 512,
        max_queued_bytes: 16 * 1024 * 1024,
        submission_timeout: Duration::from_secs(5),
        shutdown_drain_bound: Duration::from_secs(10),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: 4096,
    }
}

fn small_memtable() -> LsmConfig {
    LsmConfig {
        memtable_max_size_bytes: 4096,
        compaction_auto_trigger: false,
        ..LsmConfig::default()
    }
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// A crash image of a LIVE engine directory: every file copied as it is on disk now (the WAL lock file excluded).
fn crash_image(live: &Path, tag: &str) -> PathBuf {
    fn copy(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for e in fs::read_dir(from).unwrap().flatten() {
            let p = e.path();
            let q = to.join(e.file_name());
            if p.is_dir() {
                copy(&p, &q);
            } else if e.file_name() != "LOCK" {
                fs::copy(&p, &q).unwrap();
            }
        }
    }
    let img = temp_dir(tag);
    copy(live, &img);
    img
}

// ---------------------------------------------------------------------------------------------------------------------
// F-D1a-1 - acknowledged write lost when a freeze overtakes a lower seq's MemTable apply
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Shape {
    Put,
    Batch,
}

/// Writer A's commit unit is durable (WAL-acknowledged) but held back by the `lsm.before_apply` seam; writer B then fills the
/// MemTable, which freezes and flushes (table `max_seq` = B's seq > A's seq); A applies afterwards into the NEW MemTable and
/// its call returns `Ok` (acknowledged). A crash image taken then, and a clean stop + reopen, must both still contain A.
fn run_f_d1a_1(shape: Shape, tag: &str) {
    let _s = serial();
    let dir = temp_dir(tag);
    let engine = Arc::new(LsmEngine::open(&dir, product_wal(), pool(), small_memtable()).unwrap());
    let _g = faults::install("lsm.before_apply=nth=1:delay=3000").unwrap();

    let ea = Arc::clone(&engine);
    let a = thread::spawn(move || match shape {
        Shape::Put => ea.put(b"A-key", b"A-value"),
        Shape::Batch => ea.write_batch(&[
            WriteOp::Put {
                key: b"A-key".to_vec(),
                value: b"A-value".to_vec(),
            },
            WriteOp::Put {
                key: b"A-key-2".to_vec(),
                value: b"A-value-2".to_vec(),
            },
        ]),
    });
    // Deterministic ordering: wait until the seam has FIRED for A (A is durable and parked before its apply) - not a sleep.
    wait_until(
        "the seam to hold writer A before its MemTable apply",
        20,
        || !faults::log().is_empty(),
    );
    assert!(
        engine.snapshot_seq() >= 1,
        "A must already be durable (WAL-acknowledged)"
    );

    // B: a value larger than the MemTable bound, so its apply fills the MemTable and freezes it.
    let seq_b = engine.put(b"B-key", &vec![b'x'; 8192]).unwrap();
    wait_until("the frozen MemTable to be flushed", 30, || {
        engine.immutable_count() == 0 && engine.sstable_count() >= 1
    });
    let seq_a = a.join().unwrap().unwrap();
    assert!(
        seq_a < seq_b,
        "A ({seq_a}) must hold the lower seq than B ({seq_b})"
    );

    let image = crash_image(&dir, &format!("{tag}_img"));
    let cp = manifest::replay_readonly(&image)
        .unwrap()
        .state
        .checkpoint_seq();

    // 1. crash image (class B): recover it and ask for A.
    let rec = LsmEngine::open(&image, product_wal(), pool(), small_memtable()).unwrap();
    let got = rec.get(b"A-key").unwrap();
    let _ = rec.shutdown();
    drop(rec);
    assert_eq!(
        got.as_deref(),
        Some(&b"A-value"[..]),
        "F-D1a-1: the acknowledged write A (seq {seq_a}) is lost in the crash image; manifest checkpoint = {cp} (>= {seq_a}: \
         recovery skips A's WAL record as 'already flushed'), B (seq {seq_b}) is in the flushed table"
    );
    if matches!(shape, Shape::Batch) {
        let rec = LsmEngine::open(
            &crash_image(&dir, &format!("{tag}_img2")),
            product_wal(),
            pool(),
            small_memtable(),
        )
        .unwrap();
        assert_eq!(
            rec.get(b"A-key-2").unwrap().as_deref(),
            Some(&b"A-value-2"[..]),
            "second member of the acknowledged batch"
        );
        let _ = rec.shutdown();
    }

    // 2. graceful stop and reopen of the live directory (not a crash at all).
    let _ = engine.shutdown();
    drop(engine);
    let again = LsmEngine::open(&dir, product_wal(), pool(), small_memtable()).unwrap();
    assert_eq!(
        again.get(b"A-key").unwrap().as_deref(),
        Some(&b"A-value"[..]),
        "F-D1a-1: the acknowledged write A is lost across a GRACEFUL stop and reopen"
    );
    assert_eq!(
        again.get(b"B-key").unwrap().map(|v| v.len()),
        Some(8192),
        "B must survive too"
    );
    let _ = again.shutdown();
}

#[test]
fn f_d1a_1_put_acknowledged_before_a_freeze_that_overtook_its_apply_survives() {
    run_f_d1a_1(Shape::Put, "d1a1_put");
}

#[test]
fn f_d1a_1_batch_acknowledged_before_a_freeze_that_overtook_its_apply_survives() {
    run_f_d1a_1(Shape::Batch, "d1a1_batch");
}

/// Guard: with NO overtaking (the seam armed but harmless: the delayed writer applies before anyone else), nothing is lost.
/// Passes today; must keep passing - it keeps a fix from "passing" by dropping data in the ordinary case.
#[test]
fn guard_f_d1a_1_ordinary_sequential_writes_survive_restart() {
    let _s = serial();
    let dir = temp_dir("d1a1_guard");
    let engine = LsmEngine::open(&dir, product_wal(), pool(), small_memtable()).unwrap();
    for i in 0..120u32 {
        engine
            .put(format!("k{i:04}").as_bytes(), &[b'v'; 100])
            .unwrap();
    }
    wait_until("flushes to finish", 30, || engine.immutable_count() == 0);
    let _ = engine.shutdown();
    drop(engine);
    let again = LsmEngine::open(&dir, product_wal(), pool(), small_memtable()).unwrap();
    for i in 0..120u32 {
        assert!(
            again.get(format!("k{i:04}").as_bytes()).unwrap().is_some(),
            "k{i:04} lost"
        );
    }
    let _ = again.shutdown();
}

// ---------------------------------------------------------------------------------------------------------------------
// F-D0-1 - zero-filled WAL tail
// ---------------------------------------------------------------------------------------------------------------------

fn wal_with_three_records(tag: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(tag);
    let (mut w, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
    for i in 0..3u32 {
        w.append_sync(WalOp::Put {
            key: format!("k{i}").as_bytes(),
            value: b"value-value-value",
        })
        .unwrap();
    }
    drop(w);
    let seg = fs::read_dir(dir.join("wal"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "log"))
        .unwrap();
    (dir, seg)
}

fn append_bytes(p: &Path, b: &[u8]) {
    let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
    f.write_all(b).unwrap();
}

#[test]
fn f_d0_1_zero_filled_wal_tail_of_any_length_is_a_torn_tail() {
    let _s = serial();
    for n in [4usize, 8, 16, 64, 4096] {
        let (dir, seg) = wal_with_three_records(&format!("d01_{n}"));
        append_bytes(&seg, &vec![0u8; n]);
        let r = wal::inspect(&dir, &WalConfig::default()).unwrap();
        assert!(
            r.corrupted_segments.is_empty() && r.records.len() == 3 && r.truncated,
            "F-D0-1: a {n}-byte zero-filled tail after 3 acknowledged records must be a TORN TAIL \
             (3 records trusted, truncated, no corrupted segment); got records={} truncated={} corrupted={:?}",
            r.records.len(),
            r.truncated,
            r.corrupted_segments
        );
    }
}

#[test]
fn f_d0_1_valid_unacknowledged_frame_then_zeros_keeps_the_valid_frame() {
    let _s = serial();
    let (dir, seg) = wal_with_three_records("d01_valid_then_zeros");
    // A 4th, complete frame (unacknowledged but valid), copied from the segment's last frame, then 4096 zero bytes.
    let bytes = fs::read(&seg).unwrap();
    // each frame here is identical in size; the 3 frames follow the 24-byte segment header
    let frame_len = (bytes.len() - 24) / 3;
    let last = bytes[bytes.len() - frame_len..].to_vec();
    // re-sequence is unnecessary for the classifier test: a duplicate of a valid frame is still a valid frame
    append_bytes(&seg, &last);
    append_bytes(&seg, &vec![0u8; 4096]);
    let r = wal::inspect(&dir, &WalConfig::default()).unwrap();
    assert!(
        r.corrupted_segments.is_empty() && r.truncated && r.records.len() == 4,
        "F-D0-1: a valid frame followed by a zero-filled tail must keep all 4 valid frames and be classified torn; \
         got records={} truncated={} corrupted={:?}",
        r.records.len(),
        r.truncated,
        r.corrupted_segments
    );
}

/// Guard: damage that is NOT a trailing zero-fill must stay corruption (a fix must not make the classifier lenient about
/// mid-file damage). Passes today; must keep passing.
#[test]
fn guard_f_d0_1_damage_followed_by_valid_frames_is_still_corruption() {
    let _s = serial();
    let (dir, seg) = wal_with_three_records("d01_guard_mid");
    let mut bytes = fs::read(&seg).unwrap();
    let frame_len = (bytes.len() - 24) / 3;
    // zero the 8-byte header of frame 2; frame 3 (valid) follows it
    for b in &mut bytes[24 + frame_len..24 + frame_len + 8] {
        *b = 0;
    }
    fs::write(&seg, &bytes).unwrap();
    let r = wal::inspect(&dir, &WalConfig::default()).unwrap();
    assert!(
        !r.corrupted_segments.is_empty(),
        "zeroed header of a middle frame, valid frame after it: must be corruption"
    );
}

/// Guard: non-zero garbage that is not a complete frame must not be accepted as data (still torn/corrupt, never records).
#[test]
fn guard_f_d0_1_all_ff_tail_never_yields_extra_records() {
    let _s = serial();
    let (dir, seg) = wal_with_three_records("d01_guard_ff");
    append_bytes(&seg, &[0xFF; 8]);
    let r = wal::inspect(&dir, &WalConfig::default()).unwrap();
    assert_eq!(r.records.len(), 3);
}

// ---------------------------------------------------------------------------------------------------------------------
// F-D0-2 - zero-filled MANIFEST tail
// ---------------------------------------------------------------------------------------------------------------------

fn manifest_with_two_edits(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    let (mut m, _) = Manifest::open_after_exclusive_lock(&dir).unwrap();
    m.append_sync(ManifestEdit::AddSstable {
        id: 1,
        min_seq: 1,
        max_seq: 10,
        file_size: 100,
    })
    .unwrap();
    m.append_sync(ManifestEdit::SetCheckpoint {
        flushed_through_seq: 10,
        wal_segment_id: 1,
        wal_offset: 24,
    })
    .unwrap();
    dir
}

#[test]
fn f_d0_2_zero_filled_manifest_tail_of_any_length_is_a_torn_tail() {
    let _s = serial();
    for n in [4usize, 8, 16, 64] {
        let dir = manifest_with_two_edits(&format!("d02_{n}"));
        let before = fs::metadata(dir.join("MANIFEST")).unwrap().len();
        append_bytes(&dir.join("MANIFEST"), &vec![0u8; n]);
        let r = manifest::replay_readonly(&dir).unwrap_or_else(|e| {
            panic!("F-D0-2: a {n}-byte zero-filled MANIFEST tail must be a torn tail, but replay failed: {e}")
        });
        assert!(
            r.truncated && r.edit_count == 2 && r.valid_length == before,
            "n={n}: {r:?}"
        );
        // and the engine-side open truncates it and keeps both edits
        let (_m, replay) = Manifest::open_after_exclusive_lock(&dir).unwrap();
        assert_eq!(replay.edit_count, 2);
        assert_eq!(
            fs::metadata(dir.join("MANIFEST")).unwrap().len(),
            before,
            "tail must be truncated back"
        );
    }
}

/// Guard: a corrupt middle frame with valid frames after it stays an error.
#[test]
fn guard_f_d0_2_corrupt_middle_frame_is_still_an_error() {
    let _s = serial();
    let dir = manifest_with_two_edits("d02_guard");
    let p = dir.join("MANIFEST");
    let mut b = fs::read(&p).unwrap();
    b[8] ^= 0xFF; // first frame's body
    fs::write(&p, b).unwrap();
    assert!(manifest::replay_readonly(&dir).is_err());
}

// ---------------------------------------------------------------------------------------------------------------------
// F-D0-3 - a failed MANIFEST write is not rolled back
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn f_d0_3_failed_short_manifest_write_is_rolled_back_and_the_retry_leaves_a_readable_file() {
    let _s = serial();
    let dir = temp_dir("d03");
    let (mut m, _) = Manifest::open_after_exclusive_lock(&dir).unwrap();
    let e1 = ManifestEdit::AddSstable {
        id: 1,
        min_seq: 1,
        max_seq: 10,
        file_size: 100,
    };
    m.append_sync(e1).unwrap();
    let before = fs::metadata(dir.join("MANIFEST")).unwrap().len();

    let e2 = ManifestEdit::SetCheckpoint {
        flushed_through_seq: 10,
        wal_segment_id: 1,
        wal_offset: 24,
    };
    {
        let _g = faults::install("manifest.write=once:shortwrite=7").unwrap();
        assert!(
            m.append_sync(e2).is_err(),
            "the injected short write must surface as an error"
        );
    }
    let after_failure = fs::metadata(dir.join("MANIFEST")).unwrap().len();
    assert_eq!(
        after_failure,
        before,
        "F-D0-3: the failed append left {} stray byte(s) in the MANIFEST (no rollback)",
        after_failure.saturating_sub(before)
    );
    // the flush thread retries exactly like this
    m.append_sync(e2).unwrap();
    drop(m);
    let r = manifest::replay_readonly(&dir).unwrap_or_else(|e| {
        panic!("F-D0-3: after the failed append and its retry the MANIFEST cannot be read: {e}")
    });
    assert_eq!(r.edit_count, 2);
    assert!(!r.truncated);
}

// ---------------------------------------------------------------------------------------------------------------------
// F-D0-5 - sequence numbers restart below the checkpoint when the WAL content is missing
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn f_d0_5_seq_after_recovery_is_above_the_checkpoint_even_when_the_wal_content_is_missing() {
    let _s = serial();
    let dir = temp_dir("d05");
    let e = LsmEngine::open(&dir, product_wal(), pool(), small_memtable()).unwrap();
    for i in 0..200u32 {
        e.put(format!("a-{i:04}").as_bytes(), &[b'v'; 100]).unwrap();
    }
    wait_until("flushes to finish", 30, || e.immutable_count() == 0);
    let _ = e.shutdown();
    drop(e);
    // D0 probe H4: the WAL content is gone (the premise is a lost segment / lost directory entry; its reachability is not shown).
    for ent in fs::read_dir(dir.join("wal")).unwrap().flatten() {
        if ent.path().extension().is_some_and(|x| x == "log") {
            fs::remove_file(ent.path()).unwrap();
        }
    }
    let e = LsmEngine::open(&dir, product_wal(), pool(), small_memtable()).unwrap();
    let cp = e.checkpoint_seq();
    assert!(cp > 0, "setup: the checkpoint must be above zero");
    let seq = e.put(b"post-1", b"x").unwrap();
    assert!(
        seq > cp,
        "F-D0-5: the first write after recovery was numbered {seq}, not above the checkpoint {cp}: it will be skipped as \
         'already flushed' at the next restart"
    );
    let _ = e.shutdown();
    drop(e);
    let e = LsmEngine::open(&dir, product_wal(), pool(), small_memtable()).unwrap();
    assert_eq!(
        e.get(b"post-1").unwrap().as_deref(),
        Some(&b"x"[..]),
        "F-D0-5: an acknowledged write was lost at the restart"
    );
    let _ = e.shutdown();
}

// ---------------------------------------------------------------------------------------------------------------------
// F-D1a-2 (observation, not part of the gate): snapshot_seq() is the DURABLE watermark
// ---------------------------------------------------------------------------------------------------------------------

/// Writer A is durable (seq 1) but parked before its MemTable apply; writer B (seq 2) applies. A snapshot taken now has
/// `seq >= 2`, yet a scan at that snapshot does not contain A's key although A's commit is durable and lower than the snapshot.
/// Prints what it saw to `target/fault-points-results/observation_f_d1a_2.txt`. Not a pass/fail matrix row.
#[test]
#[ignore]
fn observe_f_d1a_2_snapshot_seq_can_exceed_the_applied_prefix() {
    let _s = serial();
    let dir = temp_dir("d1a2");
    let engine = Arc::new(LsmEngine::open(&dir, product_wal(), pool(), small_memtable()).unwrap());
    let _g = faults::install("lsm.before_apply=nth=1:delay=3000").unwrap();
    let ea = Arc::clone(&engine);
    let a = thread::spawn(move || ea.put(b"A-key", b"A-value"));
    wait_until("the seam to hold writer A", 20, || {
        !faults::log().is_empty()
    });
    let seq_b = engine.put(b"B-key", b"B-value").unwrap();
    let snap = engine.snapshot();
    let seen: Vec<String> = engine
        .range_scan(
            std::ops::Bound::Unbounded,
            std::ops::Bound::Unbounded,
            snap.seq(),
        )
        .map(|r| String::from_utf8_lossy(&r.unwrap().0).to_string())
        .collect();
    let text = format!(
        "F-D1a-2 observation: writer A is durable at seq 1 but parked before its apply; writer B got seq {seq_b}.\n\
         snapshot.seq() = {} (>= A's seq 1); keys visible at that snapshot: {seen:?}; A-key visible: {}\n",
        snap.seq(),
        seen.iter().any(|k| k == "A-key")
    );
    println!("{text}");
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("fault-points-results");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("observation_f_d1a_2.txt"), &text).unwrap();
    drop(snap);
    let _ = a.join();
    let _ = engine.shutdown();
}
