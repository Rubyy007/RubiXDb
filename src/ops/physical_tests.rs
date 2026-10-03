//! Physical-layer corruption campaign: real data directories (real WAL
//! segments, MANIFEST, SSTables written by a real engine) damaged on disk in
//! specific ways, checked with `check_physical`, and — for the cases that
//! should fail closed — opened with the real engine to prove it refuses and
//! leaves the directory byte-for-byte unchanged.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::lsm::{LsmConfig, LsmEngine};
use crate::ops::check::{check_physical, finding_codes as fc, CheckReport, Severity};
use crate::ops::integration_tests::temp_dir;
use crate::wal::{SyncMode, WalConfig};

fn wal_cfg() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(2),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

fn pool_cfg() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig {
        queue_capacity: 256,
        max_queued_bytes: 8 * 1024 * 1024,
        submission_timeout: Duration::from_secs(5),
        shutdown_drain_bound: Duration::from_secs(10),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: 4096,
    }
}

/// A directory with several live SSTables and a non-empty WAL tail.
fn template() -> PathBuf {
    let dir = temp_dir("phys_template");
    let engine = LsmEngine::open(
        &dir,
        wal_cfg(),
        pool_cfg(),
        LsmConfig {
            memtable_max_size_bytes: 48 * 1024,
            compaction_auto_trigger: false,
            ..LsmConfig::default()
        },
    )
    .unwrap();
    let value = vec![0x5Au8; 200];
    for i in 0..1200u32 {
        engine
            .put(format!("key-{i:06}").as_bytes(), &value)
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while engine.sstable_count() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(engine.sstable_count() >= 2, "template needs live sstables");
    // A few records that stay in the WAL only.
    for i in 0..20u32 {
        engine.put(format!("tail-{i:03}").as_bytes(), b"t").unwrap();
    }
    engine.shutdown();
    drop(engine);
    dir
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        let t = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &t);
        } else {
            fs::copy(&p, &t).unwrap();
        }
    }
}

fn fresh_copy(template: &Path) -> PathBuf {
    let d = temp_dir("phys_case").join("data");
    copy_dir(template, &d);
    d
}

fn snapshot_of(dir: &Path) -> BTreeMap<String, (u64, u64)> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, (u64, u64)>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let bytes = fs::read(&p).unwrap();
                out.insert(
                    p.strip_prefix(base).unwrap().to_string_lossy().to_string(),
                    (bytes.len() as u64, xxhash_rust::xxh64::xxh64(&bytes, 0)),
                );
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(dir, dir, &mut m);
    m
}

fn ssts(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir.join("sstables"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sst"))
        .collect();
    v.sort();
    v
}

fn wal_segments(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir.join("wal"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    v.sort();
    v
}

fn flip(path: &Path, offset: usize) {
    let mut b = fs::read(path).unwrap();
    b[offset] ^= 0xFF;
    fs::write(path, b).unwrap();
}

fn has(r: &CheckReport, code: &str, sev: Severity) -> bool {
    r.findings
        .iter()
        .any(|f| f.code == code && f.severity == sev)
}

fn open_result(dir: &Path) -> Result<LsmEngine, crate::EngineError> {
    LsmEngine::open(dir, wal_cfg(), pool_cfg(), LsmConfig::default())
}

#[test]
fn a_clean_directory_has_no_physical_errors_and_opens() {
    let t = template();
    let d = fresh_copy(&t);
    let r = check_physical(&d);
    assert_eq!(r.errors, 0, "{:#?}", r.findings);
    assert!(r.complete);
    // And the check itself never modifies anything.
    let before = snapshot_of(&d);
    let _ = check_physical(&d);
    assert_eq!(before, snapshot_of(&d));
    let e = open_result(&d).unwrap();
    e.shutdown();
}

#[test]
fn a_missing_live_sstable_is_detected_and_open_fails_closed_without_modifying() {
    let t = template();
    let d = fresh_copy(&t);
    fs::remove_file(&ssts(&d)[0]).unwrap();
    let r = check_physical(&d);
    assert!(
        has(&r, fc::SSTABLE_MISSING, Severity::Error),
        "{:#?}",
        r.findings
    );
    let before = snapshot_of(&d);
    let res = open_result(&d);
    // Record the engine's actual behaviour: it must not "succeed with less data".
    assert!(
        res.is_err(),
        "engine opened a directory whose manifest lists a missing sstable"
    );
    assert_eq!(
        before,
        snapshot_of(&d),
        "a refused open must not modify the directory"
    );
}

#[test]
fn a_flipped_byte_in_an_sstable_data_block_is_detected() {
    let t = template();
    let d = fresh_copy(&t);
    flip(&ssts(&d)[0], 100);
    let r = check_physical(&d);
    assert!(
        has(&r, fc::SSTABLE_CORRUPT, Severity::Error),
        "{:#?}",
        r.findings
    );
}

#[test]
fn a_truncated_sstable_is_detected() {
    let t = template();
    let d = fresh_copy(&t);
    let p = &ssts(&d)[0];
    let b = fs::read(p).unwrap();
    fs::write(p, &b[..b.len() - 100]).unwrap();
    let r = check_physical(&d);
    assert!(
        has(&r, fc::SSTABLE_SIZE_MISMATCH, Severity::Error),
        "{:#?}",
        r.findings
    );
    assert!(has(&r, fc::SSTABLE_CORRUPT, Severity::Error));
    assert!(open_result(&d).is_err());
}

#[test]
fn an_unreferenced_sstable_is_a_warning_not_an_error() {
    let t = template();
    let d = fresh_copy(&t);
    let first = ssts(&d)[0].clone();
    let extra = d.join("sstables").join("00000000000000099999.sst");
    fs::copy(&first, extra).unwrap();
    let r = check_physical(&d);
    assert!(has(&r, fc::SSTABLE_ORPHAN, Severity::Warning));
    assert_eq!(r.errors, 0, "{:#?}", r.findings);
}

#[test]
fn a_torn_manifest_tail_is_a_warning_and_mid_manifest_corruption_is_an_error() {
    let t = template();
    let d = fresh_copy(&t);
    let m = d.join("MANIFEST");
    let b = fs::read(&m).unwrap();
    fs::write(&m, &b[..b.len() - 2]).unwrap();
    let r = check_physical(&d);
    assert!(
        has(&r, fc::MANIFEST_TORN_TAIL, Severity::Warning),
        "{:#?}",
        r.findings
    );
    assert_eq!(r.errors, 0, "{:#?}", r.findings);

    let d2 = fresh_copy(&t);
    let len = fs::read(d2.join("MANIFEST")).unwrap().len();
    assert!(len > 40, "manifest needs several frames, has {len} bytes");
    flip(&d2.join("MANIFEST"), 10);
    let r2 = check_physical(&d2);
    assert!(
        r2.errors > 0 || r2.warnings > 0,
        "a flipped manifest byte must never check clean: {:#?}",
        r2.findings
    );
}

#[test]
fn a_torn_wal_tail_is_a_warning_and_a_mid_segment_bad_frame_is_corruption() {
    let t = template();
    let d = fresh_copy(&t);
    let seg = wal_segments(&d).last().unwrap().clone();
    let b = fs::read(&seg).unwrap();
    fs::write(&seg, &b[..b.len() - 3]).unwrap();
    let r = check_physical(&d);
    assert!(
        has(&r, fc::WAL_TORN_TAIL, Severity::Warning),
        "{:#?}",
        r.findings
    );
    assert_eq!(r.errors, 0, "{:#?}", r.findings);

    let d2 = fresh_copy(&t);
    let seg2 = wal_segments(&d2).last().unwrap().clone();
    let len = fs::read(&seg2).unwrap().len();
    assert!(
        len > 24 + 200,
        "wal segment needs several frames, has {len} bytes"
    );
    flip(&seg2, 24 + 20);
    let r2 = check_physical(&d2);
    assert!(
        has(&r2, fc::WAL_CORRUPT, Severity::Error),
        "{:#?}",
        r2.findings
    );
}

#[test]
fn wrong_format_versions_are_refused_and_the_directory_is_not_modified() {
    let t = template();

    // WAL segment header version (offset 8).
    let d = fresh_copy(&t);
    let seg = wal_segments(&d).last().unwrap().clone(); // the newest segment holds records recovery needs
    let mut b = fs::read(&seg).unwrap();
    b[8..12].copy_from_slice(&2u32.to_le_bytes());
    fs::write(&seg, b).unwrap();
    let r = check_physical(&d);
    assert!(r.errors > 0, "{:#?}", r.findings);
    let before = snapshot_of(&d);
    let err = crate::ops::open::open_engine_for_ops(&d)
        .err()
        .expect("the product guard must refuse");
    assert_eq!(err.code, crate::ops::codes::WAL_CORRUPT, "{err}");
    assert_eq!(
        before,
        snapshot_of(&d),
        "WAL version mismatch: the guarded open must not modify"
    );

    // SSTable footer version (footer starts 72 bytes before EOF; version at +8).
    let d = fresh_copy(&t);
    let p = ssts(&d)[0].clone();
    let mut b = fs::read(&p).unwrap();
    let at = b.len() - 72 + 8;
    b[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
    fs::write(&p, b).unwrap();
    let r = check_physical(&d);
    assert!(
        has(&r, fc::SSTABLE_CORRUPT, Severity::Error),
        "{:#?}",
        r.findings
    );
    let before = snapshot_of(&d);
    assert!(open_result(&d).is_err());
    assert_eq!(
        before,
        snapshot_of(&d),
        "SSTable version mismatch: open must not modify"
    );

    // Directory-level marker from a newer build.
    let d = fresh_copy(&t);
    fs::write(d.join("DATA_FORMAT"), "rubixdb-data-format=2\n").unwrap();
    let before = snapshot_of(&d);
    let err = crate::ops::open::open_engine_for_ops(&d)
        .err()
        .expect("must refuse");
    assert_eq!(err.code, crate::ops::format::UNSUPPORTED_DATA_FORMAT);
    assert_eq!(before, snapshot_of(&d));
}

/// CHARACTERISATION of an ENGINE defect (not asserted as desired behaviour):
/// `LsmEngine::open` discards the WAL replay summary, so a corrupt WAL
/// segment does not stop startup. The product entry points are protected by
/// `ops::format::startup_guard` (tested above). When the engine is fixed
/// (see PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md) this test must be flipped to
/// assert `is_err()`.
#[test]
fn engine_open_ignores_a_corrupt_wal_segment() {
    let t = template();
    let d = fresh_copy(&t);
    let seg = wal_segments(&d).last().unwrap().clone();
    let mut b = fs::read(&seg).unwrap();
    b[8..12].copy_from_slice(&2u32.to_le_bytes());
    fs::write(&seg, b).unwrap();
    let res = open_result(&d);
    let opened = res.is_ok();
    if let Ok(e) = res {
        e.shutdown();
    }
    assert!(
        opened,
        "engine behaviour changed: update the ADR and flip this test"
    );
}
