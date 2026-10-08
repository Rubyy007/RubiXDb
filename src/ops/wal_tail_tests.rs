//! F-07 / ADR-WAL-01 unit and library-level tests: the attestation file, its strict parser, the sequence rule, the
//! quarantine (bytes, CRC, header, partial files, injected I/O errors, idempotence), the startup guard on real WAL
//! directories (written by the real `FileWal`), the override, the refusal text, and the additions to `check`.
//! The real-binary matrix lives in `cli/tests/f07_tail_damage_integration.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ops::check::{check_physical, finding_codes as fc, Severity};
use crate::ops::codes;
use crate::ops::format::{
    startup_guard, startup_guard_with_tail_policy as guard,
    startup_guard_with_tail_policy_inner as guard_with,
};
use crate::ops::integration_tests::temp_dir;
use crate::ops::wal_tail::*;
use crate::wal::{FileWal, Wal, WalConfig, WalOp};

const SEG1: &str = "wal-00000000000000000001.log";

/// A data directory whose WAL holds `n` records (seq 1..=n) in one segment, written by the real `FileWal`.
fn wal_dir_with(n: u32) -> PathBuf {
    let dir = temp_dir("f07_wal");
    append(&dir, 0, n);
    dir
}

/// Appends `n` more records (continuing the sequence) and releases the WAL lock again.
fn append(dir: &Path, from: u32, n: u32) {
    let (mut wal, _) = FileWal::open_for_recovery(dir, WalConfig::default()).unwrap();
    for i in from..from + n {
        wal.append_sync(WalOp::Put {
            key: format!("k{i:04}").as_bytes(),
            value: b"value-bytes",
        })
        .unwrap();
    }
}

fn seg(dir: &Path) -> PathBuf {
    dir.join("wal").join(SEG1)
}

/// `(offset, end, seq)` of every frame of a segment, parsed from outside.
fn frames(path: &Path) -> Vec<(usize, usize, u64)> {
    let b = fs::read(path).unwrap();
    let mut off = 24usize;
    let mut v = Vec::new();
    while off + 8 <= b.len() {
        let len = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
        let end = off + 8 + len;
        if end > b.len() {
            break;
        }
        let seq = u64::from_le_bytes(b[off + 8..off + 16].try_into().unwrap());
        v.push((off, end, seq));
        off = end;
    }
    v
}

fn flip(path: &Path, offset: usize) {
    let mut b = fs::read(path).unwrap();
    b[offset] ^= 0x01;
    fs::write(path, b).unwrap();
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

/// Signs a four-line body the way the writer does (so a test can isolate one malformation from the CRC check).
fn signed(body: &str) -> String {
    format!("{body}crc32c={:08x}\n", crc32c::crc32c(body.as_bytes()))
}

fn attest(dir: &Path) -> Attestation {
    match write_attestation(dir) {
        AttestWrite::Written(a) => a,
        other => panic!("expected the attestation to be written, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Attestation text
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_attestation_text_is_canonical_and_round_trips() {
    let a = Attestation {
        segment: 1,
        length: 762,
        last_seq: 8,
    };
    let body = "rubixdb-wal-clean-stop=1\nsegment=1\nlength=762\nlast_seq=8\n";
    let expected = format!("{body}crc32c={:08x}\n", crc32c::crc32c(body.as_bytes()));
    assert_eq!(a.encode(), expected);
    assert_eq!(Attestation::parse(&expected), Ok(a));
    for (s, l, q) in [
        (1u64, 24u64, 0u64),
        (u64::MAX, u64::MAX, u64::MAX),
        (7, 64 * 1024 * 1024, 123_456_789),
    ] {
        let a = Attestation {
            segment: s,
            length: l,
            last_seq: q,
        };
        assert_eq!(Attestation::parse(&a.encode()), Ok(a));
    }
}

#[test]
fn the_attestation_parser_rejects_every_malformation() {
    let good = signed("rubixdb-wal-clean-stop=1\nsegment=1\nlength=762\nlast_seq=8\n");
    assert!(Attestation::parse(&good).is_ok());

    let mut bad: Vec<(&str, String)> = Vec::new();
    // missing lines (each in turn), truncation
    let lines: Vec<&str> = good.split_inclusive('\n').collect();
    for i in 0..lines.len() {
        let without: String = lines
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, l)| *l)
            .collect();
        bad.push(("missing line", without));
    }
    bad.push(("empty", String::new()));
    bad.push(("no final newline", good.trim_end_matches('\n').to_string()));
    bad.push(("cut in the middle", good[..good.len() - 5].to_string()));
    // malformed integers (re-signed, so the integer check is what fails)
    for v in [
        "x",
        "-1",
        "+1",
        "01",
        "",
        " 1",
        "1 ",
        "1.5",
        "99999999999999999999",
        "0x10",
    ] {
        bad.push((
            "bad segment",
            signed(&format!(
                "rubixdb-wal-clean-stop=1\nsegment={v}\nlength=762\nlast_seq=8\n"
            )),
        ));
        bad.push((
            "bad last_seq",
            signed(&format!(
                "rubixdb-wal-clean-stop=1\nsegment=1\nlength=762\nlast_seq={v}\n"
            )),
        ));
        bad.push((
            "bad length",
            signed(&format!(
                "rubixdb-wal-clean-stop=1\nsegment=1\nlength={v}\nlast_seq=8\n"
            )),
        ));
    }
    // semantic values
    bad.push((
        "segment 0",
        signed("rubixdb-wal-clean-stop=1\nsegment=0\nlength=762\nlast_seq=8\n"),
    ));
    bad.push((
        "length below a header",
        signed("rubixdb-wal-clean-stop=1\nsegment=1\nlength=23\nlast_seq=8\n"),
    ));
    // version
    for v in ["0", "2", "01", "1.0", "one", ""] {
        bad.push((
            "unknown version",
            signed(&format!(
                "rubixdb-wal-clean-stop={v}\nsegment=1\nlength=762\nlast_seq=8\n"
            )),
        ));
    }
    // duplicate / extra / reordered / no '='
    bad.push((
        "duplicate key",
        signed("rubixdb-wal-clean-stop=1\nsegment=1\nsegment=1\nlength=762\nlast_seq=8\n"),
    ));
    bad.push((
        "extra key",
        signed("rubixdb-wal-clean-stop=1\nsegment=1\nlength=762\nlast_seq=8\nextra=1\n"),
    ));
    bad.push((
        "reordered",
        signed("rubixdb-wal-clean-stop=1\nlength=762\nsegment=1\nlast_seq=8\n"),
    ));
    bad.push((
        "line without '='",
        signed("rubixdb-wal-clean-stop=1\nsegment\nlength=762\nlast_seq=8\n"),
    ));
    bad.push((
        "key case",
        signed("rubixdb-wal-clean-stop=1\nSegment=1\nlength=762\nlast_seq=8\n"),
    ));
    // crc
    let crc_start = good.rfind("crc32c=").unwrap() + 7;
    let mut flipped = good.clone();
    flipped.replace_range(
        crc_start..crc_start + 1,
        if &good[crc_start..crc_start + 1] == "0" {
            "1"
        } else {
            "0"
        },
    );
    bad.push(("flipped crc", flipped));
    let crc_hex = good[crc_start..crc_start + 8].to_string();
    if crc_hex.bytes().any(|c| c.is_ascii_lowercase()) {
        bad.push((
            "uppercase crc",
            good.replace(&crc_hex, &crc_hex.to_uppercase()),
        ));
    }
    bad.push((
        "short crc",
        good.replace(
            &good[crc_start..crc_start + 8],
            &good[crc_start..crc_start + 7],
        ),
    ));
    bad.push((
        "changed value, old crc",
        good.replace("last_seq=8", "last_seq=9"),
    ));
    // trailing garbage and line-ending variants
    bad.push(("trailing text", format!("{good}x")));
    bad.push(("trailing line", format!("{good}x=1\n")));
    bad.push(("trailing blank line", format!("{good}\n")));
    bad.push(("CRLF", good.replace('\n', "\r\n")));
    bad.push(("non-ASCII", good.replace("segment", "segmént")));
    for (what, text) in &bad {
        assert!(
            Attestation::parse(text).is_err(),
            "{what}: {text:?} was accepted"
        );
    }
}

#[test]
fn a_missing_unreadable_malformed_or_unknown_version_file_is_unattested_not_a_refusal() {
    let dir = wal_dir_with(5);
    let path = dir.join(ATTESTATION_FILE);
    assert_eq!(read_attestation(&dir), AttestationState::Absent);
    let good = Attestation {
        segment: 1,
        length: 24,
        last_seq: 5,
    };
    for (what, bytes) in [
        ("malformed", b"garbage\n".to_vec()),
        ("empty", Vec::new()),
        ("binary", vec![0xFF, 0xFE, 0x00, 0x01]),
        (
            "unknown version",
            signed("rubixdb-wal-clean-stop=2\nsegment=1\nlength=24\nlast_seq=99\n").into_bytes(),
        ),
        (
            "bad crc",
            good.encode().replace("crc32c=", "crc32c=0").into_bytes(),
        ),
        ("too large", vec![b'a'; 4096]),
    ] {
        fs::write(&path, bytes).unwrap();
        assert!(
            matches!(read_attestation(&dir), AttestationState::Invalid(_)),
            "{what}"
        );
        // The guard treats it as unattested: it must pass (the WAL is intact) and say so.
        let g = guard(&dir, false)
            .unwrap_or_else(|e| panic!("{what}: an unusable attestation must not refuse: {e}"));
        assert!(g.tail.unattested_note.is_some(), "{what}: no stderr note");
        assert!(
            !path.exists(),
            "{what}: the file is consumed once the guard has passed"
        );
    }
    // a directory where the file should be: unreadable
    fs::create_dir(&path).unwrap();
    assert!(matches!(
        read_attestation(&dir),
        AttestationState::Invalid(_)
    ));
}

// ---------------------------------------------------------------------------------------------------------------
// Writing the attestation
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_attestation_is_written_from_the_stopped_state_with_a_valid_crc() {
    let dir = wal_dir_with(7);
    let a = attest(&dir);
    let len = fs::metadata(seg(&dir)).unwrap().len();
    assert_eq!(
        a,
        Attestation {
            segment: 1,
            length: len,
            last_seq: 7
        }
    );
    let text = fs::read_to_string(dir.join(ATTESTATION_FILE)).unwrap();
    assert_eq!(text, a.encode());
    assert_eq!(Attestation::parse(&text), Ok(a));
    assert!(
        !dir.join("WAL_CLEAN_STOP.tmp").exists(),
        "no temp file is left behind"
    );
    // an empty WAL attests sequence 0 (it can never refuse)
    let empty = temp_dir("f07_empty");
    drop(FileWal::open_for_recovery(&empty, WalConfig::default()).unwrap());
    assert_eq!(
        attest(&empty),
        Attestation {
            segment: 1,
            length: 24,
            last_seq: 0
        }
    );
}

#[test]
fn the_attestation_is_not_written_for_a_corrupt_torn_or_inconsistent_directory() {
    // torn tail
    let torn = wal_dir_with(6);
    let len = fs::metadata(seg(&torn)).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(seg(&torn))
        .unwrap()
        .set_len(len - 3)
        .unwrap();
    assert!(matches!(
        write_attestation(&torn),
        AttestWrite::NotEligible(_)
    ));
    assert!(!torn.join(ATTESTATION_FILE).exists());
    // corrupted segment (middle frame)
    let corrupt = wal_dir_with(6);
    let f = frames(&seg(&corrupt));
    flip(&seg(&corrupt), f[2].1 - 3);
    assert!(matches!(
        write_attestation(&corrupt),
        AttestWrite::NotEligible(_)
    ));
    assert!(!corrupt.join(ATTESTATION_FILE).exists());
    // the newest segment is a header-only file after earlier ones: the final length check fails
    let rotated = wal_dir_with(4);
    let mut header = fs::read(seg(&rotated)).unwrap()[..24].to_vec();
    header[12..20].copy_from_slice(&2u64.to_le_bytes());
    fs::write(
        rotated.join("wal").join("wal-00000000000000000002.log"),
        header,
    )
    .unwrap();
    assert!(matches!(
        write_attestation(&rotated),
        AttestWrite::NotEligible(_)
    ));
    assert!(!rotated.join(ATTESTATION_FILE).exists());
    // no WAL at all
    assert!(matches!(
        write_attestation(&temp_dir("f07_nowal")),
        AttestWrite::NotEligible(_)
    ));
    // the engine still holds the WAL (exclusive lock): the read-only replay cannot run, nothing is written
    let live = temp_dir("f07_live");
    let (mut wal, _) = FileWal::open_for_recovery(&live, WalConfig::default()).unwrap();
    wal.append_sync(WalOp::Put {
        key: b"a",
        value: b"b",
    })
    .unwrap();
    assert!(matches!(
        write_attestation(&live),
        AttestWrite::NotEligible(_)
    ));
    assert!(!live.join(ATTESTATION_FILE).exists());
    drop(wal);
    assert!(matches!(write_attestation(&live), AttestWrite::Written(_)));
}

#[test]
fn a_failed_attestation_write_reports_and_leaves_nothing_that_looks_complete() {
    let dir = wal_dir_with(3);
    // the temp path is a directory: creating it as a file fails
    fs::create_dir(dir.join("WAL_CLEAN_STOP.tmp")).unwrap();
    match write_attestation(&dir) {
        AttestWrite::Failed(why) => assert!(!why.is_empty()),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(
        !dir.join(ATTESTATION_FILE).exists(),
        "a failed write must not publish an attestation"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// The sequence rule
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_sequence_rule_compares_the_reached_sequence_with_the_attested_one() {
    let a = Attestation {
        segment: 1,
        length: 100,
        last_seq: 8,
    };
    assert_eq!(
        missing_records(&a, 7, 0),
        Some((7, 1)),
        "WAL behind -> refuse, exact gap"
    );
    assert_eq!(missing_records(&a, 0, 0), Some((0, 8)));
    assert_eq!(missing_records(&a, 8, 0), None, "WAL reaches -> pass");
    assert_eq!(
        missing_records(&a, 9, 0),
        None,
        "WAL exceeds -> pass (stale attestation)"
    );
    assert_eq!(
        missing_records(&a, 3, 8),
        None,
        "checkpoint reaches -> pass"
    );
    assert_eq!(
        missing_records(&a, 3, 20),
        None,
        "checkpoint exceeds -> pass"
    );
    assert_eq!(
        missing_records(&a, 3, 5),
        Some((5, 3)),
        "max(wal, checkpoint) is what counts"
    );
    assert_eq!(
        missing_records(&Attestation { last_seq: 0, ..a }, 0, 0),
        None
    );
}

// ---------------------------------------------------------------------------------------------------------------
// The startup guard on real WAL directories
// ---------------------------------------------------------------------------------------------------------------

fn tear_last_frame_body(dir: &Path) -> (u64, u64) {
    let f = frames(&seg(dir));
    let (_, end, seq) = *f.last().unwrap();
    flip(&seg(dir), end - 3);
    (seq, f.len() as u64)
}

#[test]
fn damage_to_the_last_acknowledged_record_after_a_clean_stop_is_refused_with_the_exact_gap() {
    let dir = wal_dir_with(8);
    let a = attest(&dir);
    assert_eq!(a.last_seq, 8);
    tear_last_frame_body(&dir);
    let before = digest(&dir);
    let err = guard(&dir, false).unwrap_err();
    assert_eq!(err.code, codes::WAL_TAIL_DAMAGED);
    let text = err.to_string();
    let expected = format!(
        "WAL_TAIL_DAMAGED: the log ends at sequence 7, but the last clean shutdown recorded 8 (segment 1, {} bytes); 1 acknowledged record(s) are missing from the end of the WAL. Opening would discard them permanently. Run `rubixdb check`, restore a verified backup into a new instance, or, if you accept losing them, start once with RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL=1. The directory has not been modified.",
        a.length
    );
    assert_eq!(text, expected);
    assert_eq!(
        digest(&dir),
        before,
        "a refused start must not modify the directory"
    );
    // the second attempt is the same refusal and still changes nothing
    assert_eq!(guard(&dir, false).unwrap_err().to_string(), expected);
    assert_eq!(digest(&dir), before);
    assert!(
        !dir.join(QUARANTINE_DIR).exists(),
        "no quarantine is written by a refused start"
    );
}

#[test]
fn the_refusal_counts_sequences_not_rows() {
    // several records lost at once: the gap is attested minus reached, whatever the record contents
    let dir = wal_dir_with(10);
    let a = attest(&dir);
    let f = frames(&seg(&dir));
    // cut the file so that only 6 frames remain, then add a torn half-frame so the tail is "torn"
    let keep_end = f[5].1;
    let mut b = fs::read(seg(&dir)).unwrap();
    b.truncate(keep_end + 5);
    fs::write(seg(&dir), b).unwrap();
    let err = guard(&dir, false).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("the log ends at sequence 6"), "{msg}");
    assert!(msg.contains(&format!("recorded {}", a.last_seq)), "{msg}");
    assert!(
        msg.contains("4 acknowledged record(s) are missing"),
        "{msg}"
    );
}

#[test]
fn an_attestation_the_log_still_reaches_never_refuses() {
    // reaches exactly: only junk after the attested end (a zero tail) is quarantined, nothing refused
    let dir = wal_dir_with(5);
    attest(&dir);
    fs::OpenOptions::new()
        .append(true)
        .open(seg(&dir))
        .unwrap()
        .set_len_pad(5);
    let g = guard(&dir, false).expect("the attested prefix is intact");
    assert!(g.tail.quarantine.is_some());
    assert!(!dir.join(ATTESTATION_FILE).exists(), "consumed");
    // exceeds: the log grew after the attestation was written (stale file left behind)
    let dir = wal_dir_with(5);
    attest(&dir);
    append(&dir, 5, 3);
    let g = guard(&dir, false).expect("a stale attestation must not refuse");
    assert!(g.tail.quarantine.is_none() && g.tail.tail.is_none());
    assert!(!dir.join(ATTESTATION_FILE).exists());
    // clean, attested, nothing to do
    let dir = wal_dir_with(5);
    attest(&dir);
    let g = guard(&dir, false).unwrap();
    assert!(g.tail.unattested_note.is_none() && g.tail.override_effects.is_empty());
}

trait PadExt {
    fn set_len_pad(&mut self, n: usize);
}
impl PadExt for fs::File {
    fn set_len_pad(&mut self, n: usize) {
        use std::io::Write;
        self.write_all(&vec![0u8; n]).unwrap();
        self.sync_all().unwrap();
    }
}

#[test]
fn the_manifest_checkpoint_counts_as_reached() {
    // A real engine directory with flushed SSTables: the checkpoint is above zero.
    let dir = temp_dir("f07_ckpt");
    {
        let engine = crate::lsm::LsmEngine::open(
            &dir,
            WalConfig {
                sync_mode: crate::wal::SyncMode::GroupCommit {
                    max_wait: std::time::Duration::from_millis(2),
                    max_batch_bytes: 256 * 1024,
                },
                ..WalConfig::default()
            },
            crate::execution::batch_coordinator::BatchCoordinatorConfig {
                queue_capacity: 256,
                max_queued_bytes: 8 * 1024 * 1024,
                submission_timeout: std::time::Duration::from_secs(5),
                shutdown_drain_bound: std::time::Duration::from_secs(10),
                await_retry_budget: std::time::Duration::from_secs(5),
                max_drain_per_batch: 4096,
            },
            crate::lsm::LsmConfig {
                memtable_max_size_bytes: 48 * 1024,
                compaction_auto_trigger: false,
                ..crate::lsm::LsmConfig::default()
            },
        )
        .unwrap();
        let value = vec![0x5Au8; 200];
        for i in 0..800u32 {
            engine
                .put(format!("key-{i:06}").as_bytes(), &value)
                .unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while engine.sstable_count() < 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            engine.sstable_count() >= 1,
            "the fixture needs a flushed SSTable"
        );
        engine.shutdown();
    }
    let ckpt = crate::manifest::replay_readonly(&dir)
        .unwrap()
        .state
        .checkpoint_seq();
    assert!(ckpt > 0, "the fixture needs a checkpoint");
    // The WAL loses everything: only a header-only newest segment remains.
    let wal = dir.join("wal");
    let newest = fs::read_dir(&wal)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .max()
        .unwrap();
    let header = fs::read(&newest).unwrap()[..24].to_vec();
    for e in fs::read_dir(&wal).unwrap().flatten() {
        if e.path().extension().is_some_and(|x| x == "log") {
            fs::remove_file(e.path()).unwrap();
        }
    }
    fs::write(&newest, header).unwrap();
    let id: u64 = newest.file_name().unwrap().to_string_lossy()[4..24]
        .parse()
        .unwrap();
    let write_att = |last_seq: u64| {
        fs::write(
            dir.join(ATTESTATION_FILE),
            Attestation {
                segment: id,
                length: 24,
                last_seq,
            }
            .encode(),
        )
        .unwrap();
    };
    write_att(ckpt);
    guard(&dir, false)
        .expect("attested sequence == checkpoint: covered by the SSTables, must pass");
    write_att(ckpt + 3);
    let err = guard(&dir, false).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains(&format!("the log ends at sequence {ckpt}")),
        "{msg}"
    );
    assert!(
        msg.contains("3 acknowledged record(s) are missing"),
        "{msg}"
    );
}

#[test]
fn existing_corruption_refusals_take_precedence_and_are_unchanged() {
    // middle-frame damage: WAL_CORRUPT with the old guard's exact message, with or without an attestation/override
    let dir = wal_dir_with(8);
    attest(&dir);
    let f = frames(&seg(&dir));
    flip(&seg(&dir), f[3].1 - 3);
    let old = startup_guard(&dir).unwrap_err();
    assert_eq!(old.code, codes::WAL_CORRUPT);
    for allow in [false, true] {
        let new = guard(&dir, allow).unwrap_err();
        assert_eq!(new, old, "override={allow}: same refusal, same message");
    }
    assert!(
        dir.join(ATTESTATION_FILE).exists(),
        "a refused start leaves the attestation alone"
    );
    // header damage and a 64-byte zero tail
    let hdr = wal_dir_with(4);
    flip(&seg(&hdr), 0);
    for allow in [false, true] {
        assert_eq!(guard(&hdr, allow).unwrap_err().code, codes::WAL_CORRUPT);
    }
    let zero = wal_dir_with(4);
    attest(&zero);
    fs::OpenOptions::new()
        .append(true)
        .open(seg(&zero))
        .unwrap()
        .set_len_pad(64);
    for allow in [false, true] {
        assert_eq!(guard(&zero, allow).unwrap_err().code, codes::WAL_CORRUPT);
    }
}

#[test]
fn the_old_guard_is_unchanged_for_its_other_callers() {
    // `startup_guard` (used by `ops::open`, hence `rubixdb check` and restore) neither quarantines nor consumes
    // the attestation, and does not refuse on it.
    let dir = wal_dir_with(6);
    attest(&dir);
    tear_last_frame_body(&dir);
    let before = digest(&dir);
    startup_guard(&dir).expect("the shared guard does not know the attestation");
    assert_eq!(
        digest(&dir),
        before,
        "the shared guard must not write or delete anything"
    );
    assert!(!dir.join(QUARANTINE_DIR).exists());
}

#[test]
fn without_an_attestation_a_torn_tail_is_quarantined_reported_and_the_start_proceeds() {
    type Damage = fn(&Path);
    let flip_last_body: Damage = |d| {
        let f = frames(&seg(d));
        flip(&seg(d), f.last().unwrap().1 - 3);
    };
    let cut_ten_bytes: Damage = |d| {
        let len = fs::metadata(seg(d)).unwrap().len();
        fs::OpenOptions::new()
            .write(true)
            .open(seg(d))
            .unwrap()
            .set_len(len - 10)
            .unwrap();
    };
    for damage in [flip_last_body, cut_ten_bytes] {
        let dir = wal_dir_with(6);
        let valid_end = frames(&seg(&dir))[4].1 as u64; // the 6th record is the one damaged
        damage(&dir);
        let original = fs::read(seg(&dir)).unwrap();
        let g = guard(&dir, false).expect("unattested: the start proceeds");
        let (segment, offset, bytes) = g.tail.tail.expect("a torn tail");
        let q = g.tail.quarantine.as_ref().expect("quarantined");
        assert_eq!((segment, offset), (1, valid_end));
        assert_eq!(bytes, original.len() as u64 - offset);
        assert_eq!(g.tail.last_good_seq, 5);
        assert!(q.path.starts_with(dir.join(QUARANTINE_DIR)));
        // report channels
        let lines = g.tail.stderr_lines().join(
            "
",
        );
        assert!(
            lines.contains(&format!(
                "segment {segment}, offset {offset}, {bytes} byte(s) removed, last good sequence 5"
            )),
            "{lines}"
        );
        assert!(lines.contains(&q.path.display().to_string()), "{lines}");
        let notes = g.tail.security_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].code, "wal.tail_quarantined");
        assert_eq!(
            notes[0].object,
            format!("segment={segment} offset={offset} bytes={bytes} last_seq=5")
        );
        // the guard itself never truncates (that is the engine's existing behaviour)
        assert_eq!(fs::read(seg(&dir)).unwrap(), original);
        // the preserved bytes are exactly the removed ones
        assert_eq!(
            &fs::read(&q.path).unwrap()[Q_HEADER_LEN..],
            &original[offset as usize..]
        );
    }
}

#[test]
fn the_override_bypasses_only_the_attestation_refusal() {
    let dir = wal_dir_with(8);
    attest(&dir);
    tear_last_frame_body(&dir);
    let g = guard(&dir, true).expect("the override accepts the loss");
    assert!(matches!(
        g.tail.override_effects.as_slice(),
        [OverrideEffect::AttestationRefusal {
            segment: 1,
            attested_seq: 8,
            reached_seq: 7
        }]
    ));
    assert!(
        g.tail.quarantine.is_some(),
        "the override still quarantines"
    );
    let notes = g.tail.security_notes();
    assert!(notes.iter().any(|n| n.code == "wal.tail_override"));
    assert!(notes.iter().any(|n| n.code == "wal.tail_quarantined"));
    assert!(!dir.join(ATTESTATION_FILE).exists());
    // enabled but changing nothing: no override event
    let clean = wal_dir_with(4);
    attest(&clean);
    let g = guard(&clean, true).unwrap();
    assert!(g.tail.override_effects.is_empty());
    assert!(g.tail.security_notes().is_empty());
}

#[test]
fn the_security_notes_carry_no_bytes_and_no_secrets() {
    let dir = wal_dir_with(8);
    attest(&dir);
    tear_last_frame_body(&dir);
    let g = guard(&dir, true).unwrap();
    for n in g.tail.security_notes() {
        assert!(n.object.len() <= 128, "fits the security log field");
        assert!(
            n.object
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '=' || c == ' ' || c == '_'),
            "{}",
            n.object
        );
        assert!(
            !n.object.contains("value-bytes") && !n.object.contains("k000"),
            "{}",
            n.object
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Quarantine
// ---------------------------------------------------------------------------------------------------------------

fn tail_dir() -> (PathBuf, u64, Vec<u8>) {
    let dir = wal_dir_with(6);
    let f = frames(&seg(&dir));
    let off = f[4].1 as u64; // keep 5 frames; the 6th is the tail
    let bytes = fs::read(seg(&dir)).unwrap()[off as usize..].to_vec();
    (dir, off, bytes)
}

#[test]
fn the_quarantined_payload_is_byte_for_byte_the_removed_tail() {
    let (dir, off, expected) = tail_dir();
    let q = quarantine_tail(&dir, 1, off).unwrap();
    assert!(!q.reused);
    assert_eq!(
        (q.segment, q.offset, q.bytes),
        (1, off, expected.len() as u64)
    );
    let file = fs::read(&q.path).unwrap();
    assert_eq!(file.len(), Q_HEADER_LEN + expected.len());
    assert_eq!(
        &file[Q_HEADER_LEN..],
        &expected[..],
        "payload must equal [offset, file length) exactly"
    );
    assert_eq!(q.crc32c, crc32c::crc32c(&expected));
    let h = inspect_quarantine_file(&q.path).unwrap();
    assert_eq!(
        h,
        QuarantineHeader {
            segment: 1,
            offset: off,
            length: expected.len() as u64,
            tail_crc32c: crc32c::crc32c(&expected)
        }
    );
    // header layout: magic, version, segment, offset, length, payload crc, header crc
    assert_eq!(&file[0..8], b"RBXWTAIL");
    assert_eq!(u32::from_le_bytes(file[8..12].try_into().unwrap()), 1);
    assert_eq!(u64::from_le_bytes(file[12..20].try_into().unwrap()), 1);
    assert_eq!(u64::from_le_bytes(file[20..28].try_into().unwrap()), off);
    assert_eq!(
        u64::from_le_bytes(file[28..36].try_into().unwrap()),
        expected.len() as u64
    );
    assert_eq!(
        u32::from_le_bytes(file[36..40].try_into().unwrap()),
        crc32c::crc32c(&expected)
    );
    assert_eq!(
        u32::from_le_bytes(file[40..44].try_into().unwrap()),
        crc32c::crc32c(&file[0..40])
    );
    let name = q.path.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        name.starts_with(&format!("wal-1.{off}.")) && name.ends_with(".tail"),
        "{name}"
    );
    // the WAL itself is not touched by quarantining
    assert_eq!(
        fs::read(seg(&dir)).unwrap().len() as u64,
        off + expected.len() as u64
    );
}

#[test]
fn a_partial_or_damaged_quarantine_file_is_recognisable_and_never_looks_valid() {
    let (dir, off, _) = tail_dir();
    let q = quarantine_tail(&dir, 1, off).unwrap();
    let good = fs::read(&q.path).unwrap();
    let probe = dir.join("probe.tail");
    for (what, bytes) in [
        ("cut inside the header", good[..20].to_vec()),
        ("header only", good[..Q_HEADER_LEN].to_vec()),
        ("cut inside the payload", good[..good.len() - 1].to_vec()),
        ("extended", [good.clone(), vec![0]].concat()),
        ("payload bit flip", {
            let mut b = good.clone();
            let n = b.len();
            b[n - 1] ^= 1;
            b
        }),
        ("header bit flip", {
            let mut b = good.clone();
            b[13] ^= 1;
            b
        }),
        ("not a quarantine file", b"hello".to_vec()),
        ("empty", Vec::new()),
    ] {
        fs::write(&probe, bytes).unwrap();
        assert!(
            inspect_quarantine_file(&probe).is_err(),
            "{what} must not validate"
        );
    }
    // `check` lists the damaged entry as incomplete, never as a preserved tail
    fs::copy(&probe, dir.join(QUARANTINE_DIR).join("wal-1.1.1.tail.tmp")).unwrap();
    let listed = list_quarantine(&dir);
    assert!(listed
        .iter()
        .any(|(n, r, _)| n.ends_with(".tmp") && r.is_err()));
    assert!(listed.iter().any(|(_, r, _)| r.is_ok()));
}

#[test]
fn every_quarantine_io_failure_is_an_error_and_leaves_no_complete_looking_file() {
    for stage in [
        QuarantineStage::CreateDir,
        QuarantineStage::CreateTemp,
        QuarantineStage::Write,
        QuarantineStage::Sync,
        QuarantineStage::Rename,
    ] {
        for disk_full in [false, true] {
            let (dir, off, _) = tail_dir();
            let err = quarantine_tail_with(
                &dir,
                1,
                off,
                1,
                QuarantineFault {
                    stage: Some(stage),
                    disk_full,
                },
            )
            .unwrap_err();
            assert!(err.contains("injected failure"), "{stage:?}: {err}");
            let leftovers: Vec<_> = fs::read_dir(dir.join(QUARANTINE_DIR))
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            assert!(
                leftovers.iter().all(|n| !n.ends_with(".tail")),
                "{stage:?}: {leftovers:?}"
            );
            assert!(
                leftovers.is_empty(),
                "{stage:?}: temp files must be cleaned up: {leftovers:?}"
            );
        }
    }
}

#[test]
fn a_quarantine_that_cannot_be_written_refuses_the_start_unless_the_override_is_set() {
    for (stage, disk_full) in [
        (QuarantineStage::CreateDir, false),
        (QuarantineStage::Write, true),
        (QuarantineStage::Sync, false),
        (QuarantineStage::Rename, false),
    ] {
        let dir = wal_dir_with(6);
        tear_last_frame_body(&dir);
        let before = digest(&dir);
        let err = guard_with(
            &dir,
            false,
            QuarantineFault {
                stage: Some(stage),
                disk_full,
            },
        )
        .unwrap_err();
        assert_eq!(err.code, codes::WAL_TAIL_QUARANTINE_FAILED, "{stage:?}");
        assert!(err
            .to_string()
            .contains("RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL=1"));
        assert_eq!(
            digest(&dir),
            before,
            "{stage:?}: the WAL and every other file stay as they were"
        );
        // with the override the start proceeds, loudly, without a preserved copy
        let g = guard_with(
            &dir,
            true,
            QuarantineFault {
                stage: Some(stage),
                disk_full,
            },
        )
        .unwrap();
        assert!(g.tail.quarantine.is_none() && g.tail.quarantine_error.is_some());
        assert!(g
            .tail
            .override_effects
            .contains(&OverrideEffect::QuarantineFailure));
        assert!(g
            .tail
            .stderr_lines()
            .join("\n")
            .contains("could NOT be preserved"));
        assert!(g
            .tail
            .security_notes()
            .iter()
            .any(|n| n.code == "wal.tail_override"));
    }
    // a real I/O failure: a regular file where the quarantine directory should be
    let dir = wal_dir_with(6);
    tear_last_frame_body(&dir);
    fs::write(dir.join(QUARANTINE_DIR), b"not a directory").unwrap();
    assert_eq!(
        guard(&dir, false).unwrap_err().code,
        codes::WAL_TAIL_QUARANTINE_FAILED
    );
}

#[test]
fn a_repeated_start_reuses_the_existing_quarantine_entry() {
    let (dir, off, _) = tail_dir();
    let first = quarantine_tail_with(&dir, 1, off, 1000, QuarantineFault::default()).unwrap();
    let before = fs::read(&first.path).unwrap();
    // crash after the quarantine, before the truncate: the next start finds the same tail again
    let second = quarantine_tail_with(&dir, 1, off, 2000, QuarantineFault::default()).unwrap();
    assert!(second.reused);
    assert_eq!(second.path, first.path);
    let names: Vec<_> = fs::read_dir(dir.join(QUARANTINE_DIR))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(names.len(), 1, "no duplicate for the same evidence");
    assert_eq!(fs::read(&first.path).unwrap(), before);
    // through the guard too
    let dir = wal_dir_with(6);
    tear_last_frame_body(&dir);
    let a = guard(&dir, false).unwrap();
    let b = guard(&dir, false).unwrap();
    assert!(
        !a.tail.quarantine.as_ref().unwrap().reused && b.tail.quarantine.as_ref().unwrap().reused
    );
    assert_eq!(fs::read_dir(dir.join(QUARANTINE_DIR)).unwrap().count(), 1);
}

#[test]
fn a_complete_quarantine_entry_is_never_overwritten_with_different_bytes() {
    let (dir, off, _) = tail_dir();
    let first = quarantine_tail_with(&dir, 1, off, 5000, QuarantineFault::default()).unwrap();
    let first_bytes = fs::read(&first.path).unwrap();
    // different evidence at the same segment/offset (the tail changed), same millisecond -> the name collides
    let mut b = fs::read(seg(&dir)).unwrap();
    let n = b.len();
    b[n - 2] ^= 0x40;
    fs::write(seg(&dir), b).unwrap();
    let second = quarantine_tail_with(&dir, 1, off, 5000, QuarantineFault::default()).unwrap();
    assert!(!second.reused);
    assert_ne!(second.path, first.path, "a free name is chosen");
    assert_eq!(
        fs::read(&first.path).unwrap(),
        first_bytes,
        "the earlier entry is byte-identical"
    );
    assert!(inspect_quarantine_file(&second.path).is_ok());
    assert_ne!(first.crc32c, second.crc32c);
    // a damaged same-named file is not "valid prior evidence": it is left alone and a new entry is written
    let dir2 = tail_dir().0;
    let off2 = frames(&seg(&dir2))[4].1 as u64;
    fs::create_dir_all(dir2.join(QUARANTINE_DIR)).unwrap();
    let junk = dir2
        .join(QUARANTINE_DIR)
        .join(format!("wal-1.{off2}.7000.tail"));
    fs::write(&junk, b"junk").unwrap();
    let q = quarantine_tail_with(&dir2, 1, off2, 7000, QuarantineFault::default()).unwrap();
    assert!(!q.reused && q.path != junk);
    assert_eq!(fs::read(&junk).unwrap(), b"junk");
}

// ---------------------------------------------------------------------------------------------------------------
// rubixdb check (read-only additions)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn check_recognises_the_new_entries_lists_the_quarantine_and_keeps_wal_torn_tail_a_warning() {
    // a directory with a torn tail (so WAL_TORN_TAIL fires) plus an attestation and a quarantine entry
    let dir = wal_dir_with(6);
    let len = fs::metadata(seg(&dir)).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(seg(&dir))
        .unwrap()
        .set_len(len - 4)
        .unwrap();
    let base = check_physical(&dir);
    assert!(base
        .findings
        .iter()
        .any(|f| f.code == fc::WAL_TORN_TAIL && f.severity == Severity::Warning));

    let q = quarantine_tail(&dir, 1, frames(&seg(&dir)).last().unwrap().1 as u64).unwrap();
    fs::write(
        dir.join(ATTESTATION_FILE),
        Attestation {
            segment: 1,
            length: 24,
            last_seq: 0,
        }
        .encode(),
    )
    .unwrap();
    let with = check_physical(&dir);
    assert!(
        with.findings.iter().all(|f| !(f.code == fc::UNEXPECTED_FILE
            && (f.detail.contains(ATTESTATION_FILE) || f.detail.contains(QUARANTINE_DIR)))),
        "the new entries are known: {:#?}",
        with.findings
    );
    let listing: Vec<_> = with
        .findings
        .iter()
        .filter(|f| f.code == fc::WAL_TAIL_QUARANTINED)
        .collect();
    assert_eq!(listing.len(), 1, "{:#?}", with.findings);
    assert_eq!(listing[0].severity, Severity::Info);
    assert!(
        listing[0].detail.contains("were preserved in"),
        "{}",
        listing[0].detail
    );
    assert!(
        listing[0].detail.contains(&q.path.display().to_string()),
        "{}",
        listing[0].detail
    );
    assert!(listing[0].object.starts_with("wal-quarantine/"));
    // exit-code relevant counters are exactly what they were
    assert_eq!((with.errors, with.warnings), (base.errors, base.warnings));
    assert!(with
        .findings
        .iter()
        .any(|f| f.code == fc::WAL_TORN_TAIL && f.severity == Severity::Warning));
    // read only: the listing changed nothing
    let snap = digest(&dir);
    let _ = check_physical(&dir);
    assert_eq!(digest(&dir), snap);
}

// ---------------------------------------------------------------------------------------------------------------
// Measurement harness for `PHASE_ITEM_F07_IMPLEMENTATION.md` (not a correctness test)
// ---------------------------------------------------------------------------------------------------------------

/// Times only the F-07 paths on real WAL directories: the unchanged `startup_guard`, the new startup-only guard
/// (attested and unattested), `write_attestation`, and the quarantine write. Run with
/// `cargo test --release -p rubixdb --lib f07_path_timings -- --ignored --nocapture`.
#[test]
#[ignore = "measurement harness; run explicitly in release mode"]
fn f07_path_timings() {
    use std::time::Instant;
    fn med(mut v: Vec<f64>) -> (f64, f64, f64) {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[v.len() / 2], v[0], v[v.len() - 1])
    }
    fn report(name: &str, size_mib: u64, v: Vec<f64>) {
        let n = v.len();
        let (m, lo, hi) = med(v);
        eprintln!("F07_PERF name={name} wal_mib={size_mib} n={n} median_ms={m:.3} min_ms={lo:.3} max_ms={hi:.3}");
    }
    const N: usize = 15;
    // (value length, label): 4 KiB values are few records per MiB; 8-byte values are the worst case for replay cost,
    // which is per record (every record is decoded into owned buffers), not per byte.
    let shapes: Vec<(usize, u64, &str)> = if std::env::var("F07_PERF_TINY").is_ok() {
        vec![(8, 16, "tiny"), (8, 64, "tiny")]
    } else {
        vec![(4096, 1, "4KiB"), (4096, 16, "4KiB"), (4096, 64, "4KiB")]
    };
    for (value_len, size_mib, shape) in shapes {
        let dir = temp_dir("f07_perf");
        {
            let (mut wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
            let value = vec![0x5Au8; value_len];
            let n = (size_mib * 1024 * 1024 / (value_len as u64 + 104).min(4200)) as u32;
            for i in 0..n {
                wal.append(WalOp::Put {
                    key: format!("k{i:08}").as_bytes(),
                    value: &value,
                })
                .unwrap();
            }
            wal.sync().unwrap();
        }
        let wal_len = fs::metadata(seg(&dir)).unwrap().len();
        eprintln!(
            "F07_PERF fixture shape={shape} wal_mib={size_mib} segment_bytes={wal_len} records={}",
            frames(&seg(&dir)).len()
        );

        let mut t = Vec::new();
        for _ in 0..N {
            let s = Instant::now();
            startup_guard(&dir).unwrap();
            t.push(s.elapsed().as_secs_f64() * 1e3);
        }
        report("guard_old_startup_guard", size_mib, t);

        let mut t = Vec::new();
        for _ in 0..N {
            let s = Instant::now();
            guard(&dir, false).unwrap();
            t.push(s.elapsed().as_secs_f64() * 1e3);
        }
        report("guard_new_unattested_clean", size_mib, t);

        let mut t = Vec::new();
        for _ in 0..N {
            attest(&dir);
            let s = Instant::now();
            guard(&dir, false).unwrap();
            t.push(s.elapsed().as_secs_f64() * 1e3);
        }
        report("guard_new_attested_clean", size_mib, t);

        let mut t = Vec::new();
        for _ in 0..N {
            let s = Instant::now();
            let a = write_attestation(&dir);
            t.push(s.elapsed().as_secs_f64() * 1e3);
            assert!(matches!(a, AttestWrite::Written(_)));
        }
        report("write_attestation", size_mib, t);
        let _ = fs::remove_file(dir.join(ATTESTATION_FILE));

        // quarantine of a 1 KiB / 1 MiB tail (never more than the segment holds)
        for tail_kib in [1u64, 1024] {
            if tail_kib * 1024 >= wal_len {
                continue;
            }
            let off = wal_len - tail_kib * 1024;
            let mut t = Vec::new();
            for _ in 0..N {
                let _ = fs::remove_dir_all(dir.join(QUARANTINE_DIR));
                let s = Instant::now();
                quarantine_tail(&dir, 1, off).unwrap();
                t.push(s.elapsed().as_secs_f64() * 1e3);
            }
            report(&format!("quarantine_write_{tail_kib}KiB"), size_mib, t);
        }
        // a whole guard run that finds a torn tail (the last 10 bytes cut): replay + quarantine of that last frame
        let len = fs::metadata(seg(&dir)).unwrap().len();
        fs::OpenOptions::new()
            .write(true)
            .open(seg(&dir))
            .unwrap()
            .set_len(len - 10)
            .unwrap();
        let mut t = Vec::new();
        for _ in 0..N {
            let _ = fs::remove_dir_all(dir.join(QUARANTINE_DIR));
            let s = Instant::now();
            let g = guard(&dir, false).unwrap();
            t.push(s.elapsed().as_secs_f64() * 1e3);
            assert!(g.tail.quarantine.is_some());
        }
        report("guard_new_with_torn_tail_quarantine", size_mib, t);
        let _ = fs::remove_dir_all(&dir);
    }
}
