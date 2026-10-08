//! F-08 / ADR-SST-01 tests: the startup SSTable preflight (every open-time damage scenario of the discovery, the
//! Manifest's record of a table, the F-07 interaction, precedence) and the background data-block verification (a clean
//! pass, a finding, the throttle, cancellation, a retired table). Every directory is written by the real engine; every
//! damage is applied from outside, to the files, the way the real-binary reproducer does.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::lsm::{LsmConfig, LsmEngine};
use crate::manifest::{Manifest, ManifestEdit};
use crate::ops::codes;
use crate::ops::format::startup_guard_with_tail_policy as guard;
use crate::ops::integration_tests::temp_dir;
use crate::ops::sstable_integrity::*;
use crate::ops::wal_tail::{write_attestation, AttestWrite, ATTESTATION_FILE};
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

/// A directory written by the real engine: `puts` 200-byte values (keys `key-NNNNNN`) with a `memtable` byte budget, so
/// several SSTables are live and a few records stay in the WAL; stopped gracefully and attested like `rubixdb gui` does.
fn build(tag: &str, puts: u32, value_len: usize, memtable: usize, min_tables: usize) -> PathBuf {
    let dir = temp_dir(tag);
    let engine = LsmEngine::open(
        &dir,
        wal_cfg(),
        pool_cfg(),
        LsmConfig {
            memtable_max_size_bytes: memtable,
            compaction_auto_trigger: false,
            ..LsmConfig::default()
        },
    )
    .unwrap();
    let value = vec![0x5Au8; value_len];
    for i in 0..puts {
        engine
            .put(format!("key-{i:06}").as_bytes(), &value)
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while engine.sstable_count() < min_tables && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        engine.sstable_count() >= min_tables,
        "the fixture needs {min_tables} live sstables"
    );
    for i in 0..20u32 {
        engine.put(format!("tail-{i:03}").as_bytes(), b"t").unwrap();
    }
    engine.shutdown();
    drop(engine);
    attest(&dir);
    dir
}

/// Built once per test run and copied by each test (the fixture needs a few hundred fsynced writes).
fn small() -> PathBuf {
    static T: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    T.get_or_init(|| build("f08_small", 1500, 200, 48 * 1024, 3))
        .clone()
}

/// About 6 MiB of tables for the throttle and cancel tests.
fn big() -> PathBuf {
    static T: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    T.get_or_init(|| build("f08_big", 1500, 4096, 1024 * 1024, 4))
        .clone()
}

fn attest(dir: &Path) {
    match write_attestation(dir) {
        AttestWrite::Written(_) => {}
        other => panic!("expected the attestation to be written, got {other:?}"),
    }
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
    let d = temp_dir("f08_case").join("data");
    copy_dir(template, &d);
    d
}

fn digest(dir: &Path) -> BTreeMap<String, (u64, u64)> {
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

// --- SSTable bytes, parsed and rewritten from outside --------------------------------------------------------------

const FOOTER: usize = 72;
const F_VERSION: usize = 8;
const F_MIN_SEQ: usize = 12;
const F_BLOOM_OFF: usize = 36;
const F_BLOOM_LEN: usize = 44;
const F_INDEX_OFF: usize = 52;
const F_INDEX_LEN: usize = 60;

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn recompute_footer_crc(b: &mut [u8]) {
    let f = b.len() - FOOTER;
    let crc = crc32c::crc32c(&b[f..f + 68]);
    b[f + 68..f + 72].copy_from_slice(&crc.to_le_bytes());
}

fn flip(path: &Path, offset: usize) {
    let mut b = fs::read(path).unwrap();
    b[offset] ^= 0x01;
    fs::write(path, b).unwrap();
}

fn footer_off(path: &Path) -> usize {
    fs::metadata(path).unwrap().len() as usize - FOOTER
}

/// `(offset, length)` of data block `n` read from the table's own index.
fn data_block(path: &Path, n: usize) -> (usize, usize) {
    let b = fs::read(path).unwrap();
    let f = b.len() - FOOTER;
    let io = u64_at(&b, f + F_INDEX_OFF) as usize;
    let mut pos = io + 4;
    for i in 0.. {
        let klen = u32::from_le_bytes(b[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4 + klen;
        let off = u64_at(&b, pos) as usize;
        let len = u32::from_le_bytes(b[pos + 8..pos + 12].try_into().unwrap()) as usize;
        if i == n {
            return (off, len);
        }
        pos += 12;
    }
    unreachable!()
}

fn damage_data_block(path: &Path, n: usize) {
    let (off, _) = data_block(path, n);
    flip(path, off + 40);
}

type Mutation = fn(&Path);

fn m_footer_magic(p: &Path) {
    flip(p, footer_off(p));
}
fn m_footer_crc(p: &Path) {
    flip(p, footer_off(p) + 71);
}
fn m_index_offset(p: &Path) {
    let mut b = fs::read(p).unwrap();
    let f = b.len() - FOOTER;
    let v = u64_at(&b, f + F_INDEX_OFF) + 1;
    b[f + F_INDEX_OFF..f + F_INDEX_OFF + 8].copy_from_slice(&v.to_le_bytes());
    recompute_footer_crc(&mut b);
    fs::write(p, b).unwrap();
}
fn m_bloom(p: &Path) {
    let b = fs::read(p).unwrap();
    let f = b.len() - FOOTER;
    let off = u64_at(&b, f + F_BLOOM_OFF) as usize;
    let len = u64_at(&b, f + F_BLOOM_LEN) as usize;
    flip(p, off + len / 2);
}
fn m_index_body(p: &Path) {
    let b = fs::read(p).unwrap();
    let f = b.len() - FOOTER;
    flip(p, u64_at(&b, f + F_INDEX_OFF) as usize + 10);
}
fn m_index_structure(p: &Path) {
    let mut b = fs::read(p).unwrap();
    let f = b.len() - FOOTER;
    let io = u64_at(&b, f + F_INDEX_OFF) as usize;
    let il = u64_at(&b, f + F_INDEX_LEN) as usize;
    let klen = u32::from_le_bytes(b[io + 4..io + 8].try_into().unwrap()) as usize;
    let len_off = io + 4 + 4 + klen + 8;
    let len = u32::from_le_bytes(b[len_off..len_off + 4].try_into().unwrap()) + 1;
    b[len_off..len_off + 4].copy_from_slice(&len.to_le_bytes());
    let crc = crc32c::crc32c(&b[io..io + il - 4]);
    b[io + il - 4..io + il].copy_from_slice(&crc.to_le_bytes());
    fs::write(p, b).unwrap();
}
fn m_version_bitflip(p: &Path) {
    flip(p, footer_off(p) + F_VERSION);
}
fn m_version_2(p: &Path) {
    let mut b = fs::read(p).unwrap();
    let f = b.len() - FOOTER;
    b[f + F_VERSION..f + F_VERSION + 4].copy_from_slice(&2u32.to_le_bytes());
    recompute_footer_crc(&mut b);
    fs::write(p, b).unwrap();
}
fn m_truncated(p: &Path) {
    let len = fs::metadata(p).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(p)
        .unwrap()
        .set_len(len / 2)
        .unwrap();
}
fn m_missing(p: &Path) {
    fs::remove_file(p).unwrap();
}

fn is_sstable_refusal(code: &str) -> bool {
    [
        codes::SSTABLE_CORRUPT,
        codes::SSTABLE_MISSING,
        codes::SSTABLE_MISMATCH,
    ]
    .contains(&code)
}

// --- A. the preflight ----------------------------------------------------------------------------------------------

#[test]
fn a_healthy_directory_passes_the_preflight_and_reports_the_tables_it_validated() {
    let t = small();
    let d = fresh_copy(&t);
    let before = digest(&d);
    let report = preflight(&d).unwrap();
    assert_eq!(digest(&d), before, "the preflight is read-only");
    let on_disk = ssts(&d);
    assert!(report.tables.len() >= 3);
    assert_eq!(report.tables.len(), on_disk.len());
    for (t, p) in report.tables.iter().zip(&on_disk) {
        assert_eq!(&t.path, p);
        assert!(t.record_count > 0);
    }
    // the guard returns the same tables
    let g = guard(&d, false).unwrap();
    assert_eq!(g.sstables.tables, report.tables);
}

#[test]
fn every_open_time_damage_scenario_is_refused_before_any_file_changes_and_the_engine_agrees() {
    let t = small();
    let scenarios: [(&str, Mutation, &str); 11] = [
        ("footer magic", m_footer_magic, codes::SSTABLE_CORRUPT),
        ("footer crc", m_footer_crc, codes::SSTABLE_CORRUPT),
        (
            "footer index offset",
            m_index_offset,
            codes::SSTABLE_CORRUPT,
        ),
        ("bloom", m_bloom, codes::SSTABLE_CORRUPT),
        ("index body", m_index_body, codes::SSTABLE_CORRUPT),
        ("index structure", m_index_structure, codes::SSTABLE_CORRUPT),
        (
            "format version bit flip",
            m_version_bitflip,
            codes::SSTABLE_CORRUPT,
        ),
        ("format version 2", m_version_2, codes::SSTABLE_CORRUPT),
        // the file no longer has the size the Manifest recorded: refused by the Manifest comparison, which precedes
        // the open (the engine would say `footer: bad magic`)
        ("truncated to half", m_truncated, codes::SSTABLE_MISMATCH),
        ("missing live file", m_missing, codes::SSTABLE_MISSING),
        (
            "junk foreign table",
            |p: &Path| {
                let junk: Vec<u8> = (0..5000u32).map(|i| (i * 31 + 7) as u8).collect();
                fs::write(p.with_file_name("00000000000000000999.sst"), junk).unwrap();
            },
            codes::SSTABLE_CORRUPT,
        ),
    ];
    for (name, mutate, code) in scenarios {
        let d = fresh_copy(&t);
        let target = ssts(&d)[0].clone();
        mutate(&target);
        let before = digest(&d);
        assert!(
            before.contains_key(ATTESTATION_FILE),
            "{name}: the fixture is attested"
        );

        let err = guard(&d, false).unwrap_err();
        assert_eq!(err.code, code, "{name}: {err}");
        assert!(is_sstable_refusal(err.code));
        let text = err.to_string();
        assert!(
            text.contains(".sst"),
            "{name}: the refusal names the file: {text}"
        );
        assert!(
            text.contains("refusing to open") && text.contains("rubixdb check"),
            "{name}: {text}"
        );
        assert!(
            text.ends_with("The directory has not been modified."),
            "{name}: {text}"
        );
        assert_eq!(
            digest(&d),
            before,
            "{name}: a refused start must not modify any file (WAL_CLEAN_STOP included)"
        );
        // a second start gives the same refusal and still changes nothing
        assert_eq!(guard(&d, false).unwrap_err(), err, "{name}");
        assert_eq!(digest(&d), before, "{name}");

        // the preflight never accepts what the engine refuses
        let engine_copy = fresh_copy(&d);
        let res = LsmEngine::open(&engine_copy, wal_cfg(), pool_cfg(), LsmConfig::default());
        assert!(res.is_err(), "{name}: the engine must also refuse");
    }
}

#[test]
fn the_engine_text_is_carried_verbatim_for_a_damaged_table() {
    let t = small();
    let d = fresh_copy(&t);
    let target = ssts(&d)[0].clone();
    m_footer_magic(&target);
    let err = guard(&d, false).unwrap_err();
    let engine_err = crate::sstable::SsTable::open(&target, 1).unwrap_err();
    let wrapped = format!(
        "corruption: sstable {}: footer: bad magic",
        target.display()
    );
    assert_eq!(
        engine_err.to_string(),
        "corruption: footer: bad magic".to_string()
    );
    assert!(err.detail.starts_with(&wrapped), "{}", err.detail);
    // and the engine's own message for the same directory is that same text
    let copy = fresh_copy(&d);
    let engine = LsmEngine::open(&copy, wal_cfg(), pool_cfg(), LsmConfig::default())
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(engine.starts_with("corruption: sstable "), "{engine}");
    assert!(engine.ends_with("footer: bad magic"), "{engine}");
}

#[test]
fn a_valid_but_wrong_file_is_refused_by_size_and_by_sequence_range_where_the_engine_accepts_it() {
    let t = small();

    // (h) table 1 replaced by a byte copy of table 2: a perfectly valid file of another size and range
    let d = fresh_copy(&t);
    let s = ssts(&d);
    fs::copy(&s[1], &s[0]).unwrap();
    let before = digest(&d);
    let err = guard(&d, false).unwrap_err();
    assert_eq!(err.code, codes::SSTABLE_MISMATCH, "{err}");
    // the fixture's tables are the same size, so on this directory it is the sequence range that differs; the size
    // wording is covered by the truncated file and the recorded-size test
    assert!(
        err.detail.contains("bytes but the Manifest recorded")
            || err.detail.contains("sequence range"),
        "{err}"
    );
    assert_eq!(digest(&d), before);
    // the gap this closes: the engine opens it without complaint
    let eng_copy = fresh_copy(&d);
    let e = LsmEngine::open(&eng_copy, wal_cfg(), pool_cfg(), LsmConfig::default())
        .expect("the engine accepts a valid file that is not the one the Manifest recorded");
    e.shutdown();

    // the same size, a different sequence range (footer min_seq + 1, CRC recomputed): a valid file the engine opens
    let d = fresh_copy(&t);
    let target = ssts(&d)[0].clone();
    let mut b = fs::read(&target).unwrap();
    let f = b.len() - FOOTER;
    let v = u64_at(&b, f + F_MIN_SEQ) + 1;
    b[f + F_MIN_SEQ..f + F_MIN_SEQ + 8].copy_from_slice(&v.to_le_bytes());
    recompute_footer_crc(&mut b);
    fs::write(&target, b).unwrap();
    let before = digest(&d);
    let err = guard(&d, false).unwrap_err();
    assert_eq!(err.code, codes::SSTABLE_MISMATCH, "{err}");
    assert!(err.detail.contains("sequence range"), "{err}");
    assert_eq!(digest(&d), before);
    let eng_copy = fresh_copy(&d);
    LsmEngine::open(&eng_copy, wal_cfg(), pool_cfg(), LsmConfig::default())
        .expect("the engine accepts it")
        .shutdown();
}

#[test]
fn a_recorded_size_of_zero_means_unknown_and_is_not_compared() {
    let t = small();
    let live = crate::manifest::replay_readonly(&t).unwrap().state;
    // a directory whose Manifest records the right ranges but size 0 (the engine's `unwrap_or(0)` when its own
    // metadata call failed), then one that records a wrong non-zero size
    for (recorded_size_delta, expect_ok) in [(None, true), (Some(1u64), false)] {
        let d = temp_dir("f08_zero").join("data");
        fs::create_dir_all(d.join("sstables")).unwrap();
        for id in live.live_sstables.keys() {
            let name = crate::sstable::sstable_filename(*id);
            fs::copy(
                t.join("sstables").join(&name),
                d.join("sstables").join(&name),
            )
            .unwrap();
        }
        {
            let (mut m, _) = Manifest::open_after_exclusive_lock(&d).unwrap();
            for (id, e) in &live.live_sstables {
                m.append_sync(ManifestEdit::AddSstable {
                    id: *id,
                    min_seq: e.min_seq,
                    max_seq: e.max_seq,
                    file_size: recorded_size_delta.map_or(0, |dlt| e.file_size + dlt),
                })
                .unwrap();
            }
        }
        let r = preflight(&d);
        assert_eq!(r.is_ok(), expect_ok, "{r:?}");
        if !expect_ok {
            assert_eq!(r.unwrap_err().code, codes::SSTABLE_MISMATCH);
        }
    }
}

#[test]
fn an_unrecorded_valid_table_is_left_to_the_engine_and_an_unrecorded_damaged_one_is_refused() {
    let t = small();
    // valid, never recorded: the engine adopts it (LSM Spec 7.2), so the preflight must not refuse it
    let d = fresh_copy(&t);
    let s = ssts(&d);
    fs::copy(&s[0], d.join("sstables").join("00000000000000000999.sst")).unwrap();
    let r = preflight(&d).unwrap();
    assert_eq!(
        r.tables.len(),
        s.len(),
        "only Manifest-live tables are listed"
    );
    // damaged, never recorded
    let d = fresh_copy(&t);
    fs::write(
        d.join("sstables").join("00000000000000000999.sst"),
        vec![9u8; 3000],
    )
    .unwrap();
    let before = digest(&d);
    let err = preflight(&d).unwrap_err();
    assert_eq!(err.code, codes::SSTABLE_CORRUPT);
    assert!(err.detail.contains("00000000000000000999.sst"), "{err}");
    assert_eq!(digest(&d), before);
    // temp files and non-table names are the engine's, never refused here
    let d = fresh_copy(&t);
    fs::write(
        d.join("sstables").join("00000000000000000998.sst.tmp"),
        b"partial",
    )
    .unwrap();
    fs::write(d.join("sstables").join("notes.txt"), b"x").unwrap();
    preflight(&d).unwrap();
}

#[test]
fn an_unreadable_manifest_refuses_with_its_own_message_and_a_missing_one_is_a_fresh_directory() {
    let t = small();
    let d = fresh_copy(&t);
    // a fresh directory: nothing to check
    let fresh = temp_dir("f08_fresh");
    assert!(preflight(&fresh).unwrap().tables.is_empty());
    // a manifest with garbage in the middle of its frames
    let m = d.join("MANIFEST");
    let mut b = fs::read(&m).unwrap();
    assert!(b.len() > 64);
    b[20] ^= 0xFF;
    fs::write(&m, b).unwrap();
    let before = digest(&d);
    let err = preflight(&d).unwrap_err();
    assert_eq!(err.code, codes::ENGINE, "{err}");
    assert!(
        err.detail.ends_with("The directory has not been modified."),
        "{err}"
    );
    assert_eq!(digest(&d), before);
}

// --- ordering and the F-07 interaction -----------------------------------------------------------------------------

fn newest_wal_segment(dir: &Path) -> PathBuf {
    let mut v: Vec<PathBuf> = fs::read_dir(dir.join("wal"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    v.sort();
    v.pop().unwrap()
}

/// Flips a byte in the body of the last frame of the newest WAL segment (the F-07 "damaged tail" case).
fn tear_last_wal_frame(dir: &Path) {
    let seg = newest_wal_segment(dir);
    let b = fs::read(&seg).unwrap();
    let mut off = 24usize;
    let mut last_end = 0usize;
    while off + 8 <= b.len() {
        let len = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
        let end = off + 8 + len;
        if end > b.len() {
            break;
        }
        last_end = end;
        off = end;
    }
    assert!(last_end > 24);
    flip(&seg, last_end - 3);
}

#[test]
fn a_refused_sstable_start_leaves_wal_clean_stop_untouched_so_the_next_start_is_still_attested() {
    let t = small();
    let d = fresh_copy(&t);
    let attestation_before = fs::read(d.join(ATTESTATION_FILE)).unwrap();
    let original = fs::read(&ssts(&d)[0]).unwrap();
    let target = ssts(&d)[0].clone();

    // the SSTable refusal does not consume the attestation (before F-08 it was consumed by the F-07 guard first)
    m_bloom(&target);
    let err = guard(&d, false).unwrap_err();
    assert!(is_sstable_refusal(err.code), "{err}");
    assert_eq!(
        fs::read(d.join(ATTESTATION_FILE)).unwrap(),
        attestation_before,
        "WAL_CLEAN_STOP is byte-identical after a refused start"
    );

    // the operator repairs the table and damages the last WAL record: the next start is refused by the attestation
    fs::write(&target, original).unwrap();
    tear_last_wal_frame(&d);
    let err = guard(&d, false).unwrap_err();
    assert_eq!(
        err.code,
        codes::WAL_TAIL_DAMAGED,
        "the attestation survived the refused start: {err}"
    );
    assert_eq!(
        fs::read(d.join(ATTESTATION_FILE)).unwrap(),
        attestation_before
    );
}

#[test]
fn wal_corrupt_keeps_its_precedence_and_a_damaged_table_is_decided_before_the_tail_policy() {
    let t = small();

    // WAL_CORRUPT (a damaged middle frame / header) and a damaged table together: the WAL refusal is reported, as before
    let d = fresh_copy(&t);
    m_footer_magic(&ssts(&d)[0]);
    let seg = newest_wal_segment(&d);
    flip(&seg, 0); // segment header magic
    let err = guard(&d, false).unwrap_err();
    assert_eq!(err.code, codes::WAL_CORRUPT, "{err}");

    // a damaged table and a damaged WAL tail together: the SSTable refusal is decided first (ADR-SST-01 section A:
    // the preflight is before the tail policy, which is the first mutation), and nothing has changed
    let d = fresh_copy(&t);
    m_footer_magic(&ssts(&d)[0]);
    tear_last_wal_frame(&d);
    let before = digest(&d);
    let err = guard(&d, false).unwrap_err();
    assert!(is_sstable_refusal(err.code), "{err}");
    assert_eq!(digest(&d), before);
    // no override can bypass an SSTable refusal
    let err = guard(&d, true).unwrap_err();
    assert!(is_sstable_refusal(err.code), "{err}");
    assert_eq!(digest(&d), before);
}

#[test]
fn an_open_close_open_cycle_with_writes_never_refuses_a_healthy_directory() {
    let dir = temp_dir("f08_cycle");
    for round in 0..4u32 {
        let g = guard(&dir, false).unwrap_or_else(|e| panic!("round {round}: {e}"));
        let engine = LsmEngine::open(
            &dir,
            wal_cfg(),
            pool_cfg(),
            LsmConfig {
                memtable_max_size_bytes: 24 * 1024,
                compaction_auto_trigger: false,
                ..LsmConfig::default()
            },
        )
        .unwrap();
        if round == 0 {
            assert!(
                g.sstables.tables.is_empty(),
                "a fresh directory has no tables"
            );
        } else {
            assert!(
                !g.sstables.tables.is_empty(),
                "round {round}: the previous rounds left tables"
            );
        }
        let value = vec![0x33u8; 200];
        for i in 0..600u32 {
            engine
                .put(format!("r{round}-{i:05}").as_bytes(), &value)
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while engine.sstable_count() < (round as usize + 1) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        engine.shutdown();
        drop(engine);
        attest(&dir);
    }
    // the guard's list is exactly the engine's live set
    let g = guard(&dir, false).unwrap();
    let e = LsmEngine::open(&dir, wal_cfg(), pool_cfg(), LsmConfig::default()).unwrap();
    let mut ids: Vec<u64> = g.sstables.tables.iter().map(|t| t.id).collect();
    ids.sort_unstable();
    let mut live = e.live_sstable_ids();
    live.sort_unstable();
    assert_eq!(ids, live);
    e.shutdown();
}

#[test]
fn directories_the_engine_wrote_through_flush_compaction_and_adoption_always_pass() {
    let dir = temp_dir("f08_compat");
    let cfg = LsmConfig {
        memtable_max_size_bytes: 16 * 1024,
        compaction_auto_trigger: true,
        compaction_trigger_count: 3,
        ..LsmConfig::default()
    };
    let engine = LsmEngine::open(&dir, wal_cfg(), pool_cfg(), cfg.clone()).unwrap();
    let value = vec![0x44u8; 200];
    for i in 0..4000u32 {
        // overwrites and deletes so compaction output is not just a concatenation
        engine
            .put(format!("key-{:05}", i % 700).as_bytes(), &value)
            .unwrap();
        if i % 9 == 0 {
            engine
                .delete(format!("key-{:05}", (i * 7) % 700).as_bytes())
                .unwrap();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while engine.compaction_metrics().cycles_completed < 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        engine.compaction_metrics().cycles_completed >= 1,
        "compaction must have produced an output table"
    );
    engine.shutdown();
    drop(engine);
    attest(&dir);
    let g = guard(&dir, false).expect("flush and compaction outputs pass the Manifest comparison");
    assert!(!g.sstables.tables.is_empty());
    attest(&dir);

    // adoption: a valid unrecorded table, adopted by the next open with its own range and size
    let live = ssts(&dir);
    fs::copy(
        &live[0],
        dir.join("sstables").join("00000000000000009999.sst"),
    )
    .unwrap();
    guard(&dir, false).unwrap();
    let engine = LsmEngine::open(&dir, wal_cfg(), pool_cfg(), cfg).unwrap();
    engine.shutdown();
    drop(engine);
    attest(&dir);
    let g = guard(&dir, false).expect("an adopted table is recorded with its own size and range");
    assert!(g
        .sstables
        .tables
        .iter()
        .any(|t| t.path.ends_with("00000000000000009999.sst")));
}

// --- B. the background verification --------------------------------------------------------------------------------

fn run(
    dir: &Path,
    mib: u64,
    tables: &[PreflightTable],
    integrity: &SstableIntegrity,
) -> Vec<DamagedTable> {
    integrity.begin(tables.len() as u64);
    let mut found = Vec::new();
    run_verification(integrity, dir, tables, mib, &mut |d| found.push(d.clone()));
    found
}

#[test]
fn a_clean_pass_reads_every_block_and_completes() {
    let t = small();
    let d = fresh_copy(&t);
    let tables = preflight(&d).unwrap().tables;
    let i = SstableIntegrity::default();
    assert_eq!(i.state(), VerificationState::Disabled);
    let found = run(&d, 1024, &tables, &i);
    assert!(found.is_empty());
    let s = i.snapshot();
    assert_eq!(s.state, VerificationState::Complete);
    assert_eq!(s.tables_total, tables.len() as u64);
    assert_eq!(s.tables_verified, s.tables_total);
    assert!(s.damaged.is_empty());
    let sizes: u64 = ssts(&d)
        .iter()
        .map(|p| fs::metadata(p).unwrap().len())
        .sum();
    // every block of every table was read; the accounting is blocks x average block size of the file
    assert!(s.bytes_verified <= sizes);
    assert!(
        s.bytes_verified as f64 > 0.99 * sizes as f64,
        "{} of {sizes}",
        s.bytes_verified
    );
}

#[test]
fn a_data_block_damaged_before_the_pass_reaches_it_is_found_and_the_other_tables_still_verify() {
    let t = small();
    let d = fresh_copy(&t);
    let tables = preflight(&d).unwrap().tables;
    // damage a middle block of the SECOND table after the preflight, before the pass: only the pass can see it
    let victim = &tables[1];
    damage_data_block(&victim.path, 1);
    let i = SstableIntegrity::default();
    let found = run(&d, 1024, &tables, &i);
    assert_eq!(found.len(), 1, "{found:?}");
    let f = &found[0];
    assert_eq!(f.id, victim.id);
    assert_eq!(
        f.path,
        format!("sstables/{}", crate::sstable::sstable_filename(victim.id))
    );
    assert!(
        f.records_before_failure > 0 && f.records_before_failure < f.records_total,
        "{f:?}"
    );
    assert_eq!(f.records_total, victim.record_count);
    let s = i.snapshot();
    assert_eq!(s.state, VerificationState::Damaged);
    assert_eq!(s.damaged, found);
    assert_eq!(
        s.tables_verified, s.tables_total,
        "the pass went on to the tables after the damaged one"
    );
    // the stderr line and the security object carry the file and counts only: never a key or a value
    let line = f.stderr_line();
    assert!(
        line.contains(&f.path) && line.contains("rubixdb check"),
        "{line}"
    );
    let obj = f.security_object();
    assert_eq!(
        obj,
        format!("table={} records_before={}", f.id, f.records_before_failure)
    );
    for text in [&line, &obj] {
        assert!(!text.contains("key-") && !text.contains("tail-"), "{text}");
    }
}

#[test]
fn damage_that_appears_while_the_pass_is_running_is_reported_as_soon_as_it_is_found() {
    // ~6 MiB of tables at 4 MiB/s: the pass runs about 1.5 s. Damage a block of the second-to-last table while the
    // first one is being read; the state must be `damaged` while the last table is still to be read, not only at the end.
    let t = big();
    let d = fresh_copy(&t);
    let tables = preflight(&d).unwrap().tables;
    assert!(tables.len() >= 4);
    let victim = tables[tables.len() - 2].path.clone();
    let i = std::sync::Arc::new(SstableIntegrity::default());
    i.begin(tables.len() as u64);
    let runner = {
        let i = i.clone();
        let tables = tables.clone();
        let d = d.clone();
        std::thread::spawn(move || {
            run_verification(&i, &d, &tables, 4, &mut |_| {});
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(i.state(), VerificationState::Running);
    damage_data_block(&victim, 1);
    let deadline = Instant::now() + Duration::from_secs(30);
    while i.state() != VerificationState::Damaged && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(i.state(), VerificationState::Damaged);
    let s = i.snapshot();
    assert!(
        s.tables_verified < s.tables_total,
        "`damaged` was reported only when the pass had finished: {s:?}"
    );
    assert_eq!(s.damaged.len(), 1);
    runner.join().unwrap();
    assert_eq!(
        i.state(),
        VerificationState::Damaged,
        "the pass ends damaged, not complete"
    );
}

#[test]
fn the_pass_is_throttled_to_its_budget() {
    // about 6 MiB of tables; at 12 MiB/s the pass must take about half a second, not milliseconds
    let t = big();
    let d = fresh_copy(&t);
    let tables = preflight(&d).unwrap().tables;
    let bytes: u64 = ssts(&d)
        .iter()
        .map(|p| fs::metadata(p).unwrap().len())
        .sum();
    assert!(bytes > 5 * 1024 * 1024, "{bytes}");
    let mib = 12u64;
    let expected = bytes as f64 / (mib * 1024 * 1024) as f64;

    let fast = SstableIntegrity::default();
    let t0 = Instant::now();
    run(&d, 1024, &tables, &fast);
    let unthrottled = t0.elapsed().as_secs_f64();

    let slow = SstableIntegrity::default();
    let t0 = Instant::now();
    run(&d, mib, &tables, &slow);
    let throttled = t0.elapsed().as_secs_f64();
    eprintln!("f08 throttle: {bytes} bytes, budget {mib} MiB/s, expected {expected:.3} s, throttled {throttled:.3} s, unthrottled (1024 MiB/s) {unthrottled:.3} s");
    assert!(
        throttled >= 0.9 * expected,
        "{throttled} s < 90% of {expected} s"
    );
    assert!(
        throttled <= expected + 1.0,
        "{throttled} s is far above {expected} s"
    );
    assert_eq!(slow.state(), VerificationState::Complete);
}

#[test]
fn a_cancel_stops_the_pass_at_the_next_block() {
    let t = big();
    let d = fresh_copy(&t);
    let tables = preflight(&d).unwrap().tables;
    let i = std::sync::Arc::new(SstableIntegrity::default());
    i.begin(tables.len() as u64);
    let h = {
        let i = i.clone();
        let d = d.clone();
        std::thread::spawn(move || run_verification(&i, &d, &tables, 1, &mut |_| {}))
    };
    std::thread::sleep(Duration::from_millis(150));
    let t0 = Instant::now();
    let at_cancel = i.snapshot().bytes_verified;
    i.request_cancel();
    h.join().unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "stopped in {:?}",
        t0.elapsed()
    );
    let s = i.snapshot();
    assert_ne!(
        s.state,
        VerificationState::Complete,
        "a cancelled pass is never reported complete"
    );
    assert!(s.tables_verified < s.tables_total);
    assert!(i.cancel_requested());
    // it stopped at the next block: not even the rest of the table it was reading was scanned (a table is ~1.5 MB; the
    // allowance is one pace step plus a block of slack)
    assert!(
        s.bytes_verified <= at_cancel + 512 * 1024,
        "{} bytes verified at cancel, {} after the join",
        at_cancel,
        s.bytes_verified
    );
}

#[test]
fn a_table_retired_after_the_preflight_is_skipped_and_one_damaged_after_it_is_a_finding() {
    let t = small();
    let d = fresh_copy(&t);
    let tables = preflight(&d).unwrap().tables;
    fs::remove_file(&tables[0].path).unwrap(); // a compaction retired it
    flip(&tables[1].path, footer_off(&tables[1].path)); // the footer was damaged after the start
    let i = SstableIntegrity::default();
    let found = run(&d, 1024, &tables, &i);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].id, tables[1].id);
    assert_eq!(found[0].records_before_failure, 0);
    let s = i.snapshot();
    assert_eq!(s.tables_verified, s.tables_total);
    assert_eq!(s.state, VerificationState::Damaged);
}

#[test]
fn the_budget_setting_is_parsed_strictly() {
    let p = |v| parse_verify_mib_per_sec("VAR", v);
    assert_eq!(p(None), Ok(DEFAULT_VERIFY_MIB_PER_SEC));
    assert_eq!(DEFAULT_VERIFY_MIB_PER_SEC, 64);
    assert_eq!(p(Some("0")), Ok(0));
    assert_eq!(p(Some("1024")), Ok(1024));
    for bad in [
        "", "1025", "-1", "+5", " 5", "5 ", "1.5", "1e3", "x", "0x10",
    ] {
        let e = p(Some(bad)).unwrap_err();
        assert!(e.contains("VAR") && e.contains("1024"), "{bad:?}: {e}");
    }
}

// --- evidence tool -------------------------------------------------------------------------------------------------

/// Not a regression test (ignored): runs the preflight, `check_physical` and an engine open (on a copy) over every data
/// directory listed in the file named by `F08_SWEEP_LIST` (one path per line) and writes one TSV row per directory to
/// `F08_SWEEP_OUT`: path, preflight result (live-table count or refusal code), `check_physical` error count, whether the
/// engine opened the copy, and the refusal detail. Used to confirm, on directories real runs left behind, that the
/// Manifest comparison refuses nothing the engine and `rubixdb check` accept (ADR-SST-01, Migration).
#[test]
#[ignore = "evidence sweep: F08_SWEEP_LIST=<file of data dirs> F08_SWEEP_OUT=<tsv>"]
fn sweep_existing_directories() {
    let list = std::env::var("F08_SWEEP_LIST").expect("F08_SWEEP_LIST");
    let out = std::env::var("F08_SWEEP_OUT").expect("F08_SWEEP_OUT");
    let mut rows = String::new();
    for line in fs::read_to_string(list).unwrap().lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let dir = PathBuf::from(line);
        let pre = preflight(&dir);
        let phys = crate::ops::check::check_physical(&dir);
        let copy = fresh_copy(&dir);
        let engine = LsmEngine::open(&copy, wal_cfg(), pool_cfg(), LsmConfig::default());
        let opens = engine.is_ok();
        if let Ok(e) = engine {
            e.shutdown();
        }
        let _ = fs::remove_dir_all(copy.parent().unwrap());
        rows.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            dir.display(),
            pre.as_ref()
                .map(|r| r.tables.len().to_string())
                .unwrap_or_else(|e| e.code.to_string()),
            phys.errors,
            if opens { "opens" } else { "refused" },
            pre.as_ref()
                .err()
                .map(|e| e.detail.chars().take(200).collect::<String>())
                .unwrap_or_default()
        ));
    }
    fs::write(out, rows).unwrap();
}
