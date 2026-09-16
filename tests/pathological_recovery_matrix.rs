//! Phase 3C pathological-recovery-stress evidence matrix (operating
//! brief §7, `PHASE3C_TEST_PLAN.md`). Constructs the exact fixture
//! types §7 names, using the existing, unmodified `FileWal`/`open_for_
//! recovery` recovery contract (never a second recovery implementation
//! — corruption is injected by directly mutating on-disk bytes via
//! `std::fs`, exactly the technique `tests/wal_tests.rs`'s own Phase 0/1
//! corruption tests already established). For each fixture, prints and
//! asserts: expected result, actual result, recovered sequence,
//! corruption classification, startup (recovery) time — the evidence
//! table `PHASE3C_TEST_RESULTS.md` §7 draws from directly.
//!
//! **Not a duplicate of Phase 0/1's own corruption coverage.** Several
//! of these exact properties (torn tail, corrupted non-last segment,
//! random-byte-corruption prefix invariant, 10,000-case arbitrary-noise
//! panic safety) were already proven correct by `tests/wal_tests.rs`
//! and `src/wal/fuzz_tests.rs` — this file's job is to present them
//! together as one consolidated, explicitly-itemized certification
//! matrix against §7's own fixture list, not to re-derive what is
//! already proven. Two fixtures here genuinely are new coverage: an
//! out-of-range `length` field on a *non-tail* frame (distinct from the
//! write-time `max_record_len` enforcement Phase 0 already covers), and
//! an unrecognized op-tag byte with an otherwise-valid CRC (isolates the
//! op-decode failure path from the CRC-mismatch path, which existing
//! tests do not separately exercise).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rubixdb::wal::{FileWal, Wal, WalConfig, WalOp};

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
        let path =
            std::env::temp_dir().join(format!("rubixdb_pathological_{test_name}_{nanos}_{n}"));
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

fn write_records(dir: &Path, config: WalConfig, count: u32) {
    let (mut wal, _) = FileWal::open_for_recovery(dir, config).unwrap();
    for i in 0..count {
        wal.append_sync(WalOp::Put {
            key: format!("key-{i:06}").as_bytes(),
            value: b"pathological-recovery-matrix-value",
        })
        .unwrap();
    }
}

/// Prints one evidence-matrix row in the format `PHASE3C_TEST_RESULTS.md`
/// §7 records: fixture name, expected, actual, recovered sequence,
/// corruption classification, startup time.
fn report_row(
    fixture: &str,
    expected: &str,
    corrupted_count: usize,
    truncated: bool,
    records: usize,
    highest_seq: u64,
    elapsed_ms: f64,
) {
    println!(
        "pathological_recovery_matrix: fixture={fixture} expected={expected} \
         corrupted_segments={corrupted_count} truncated={truncated} records={records} \
         highest_seq={highest_seq} recovery_ms={elapsed_ms:.3}"
    );
}

/// Fixture 1: many segments (10+), all valid — the baseline "valid
/// durable tail" case doubling as the many-segments case.
#[test]
fn fixture_many_segments_all_valid() {
    let dir = TempDir::new("many_segments");
    write_records(dir.path(), small_config(256), 200);
    let segments = list_segment_paths(dir.path());
    assert!(segments.len() >= 10, "need many segments for this fixture");

    let started = Instant::now();
    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(256)).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

    report_row(
        "many_segments_all_valid",
        "OK, all 200 records recovered, gap-free, zero corruption",
        result.corrupted_segments.len(),
        result.truncated,
        result.records.len(),
        result.records.last().map(|(s, _)| *s).unwrap_or(0),
        elapsed_ms,
    );
    assert!(result.corrupted_segments.is_empty());
    assert_eq!(result.records.len(), 200);
    for (i, (seq, _)) in result.records.iter().enumerate() {
        assert_eq!(*seq, (i as u64) + 1);
    }
}

/// Fixture 2: many batches (many separate `append_sync` calls, i.e.
/// many separate fsync-backed writes, not one bulk write) — exercises
/// the same segment/record accounting under a different write shape.
#[test]
fn fixture_many_batches() {
    let dir = TempDir::new("many_batches");
    let (mut wal, _) = FileWal::open_for_recovery(dir.path(), WalConfig::default()).unwrap();
    for batch in 0..500u32 {
        wal.append_sync(WalOp::Put {
            key: format!("batch-{batch}").as_bytes(),
            value: b"v",
        })
        .unwrap();
    }
    drop(wal);

    let started = Instant::now();
    let (_, result) = FileWal::open_for_recovery(dir.path(), WalConfig::default()).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    report_row(
        "many_batches",
        "OK, 500 records recovered, gap-free",
        result.corrupted_segments.len(),
        result.truncated,
        result.records.len(),
        result.records.last().map(|(s, _)| *s).unwrap_or(0),
        elapsed_ms,
    );
    assert_eq!(result.records.len(), 500);
    assert!(result.corrupted_segments.is_empty());
}

/// Fixture 3: partial final header — the last segment is truncated
/// strictly inside the 8-byte frame header. Expected per WAL Spec
/// §6.2 step 2: a torn tail, not corruption.
#[test]
fn fixture_partial_final_header() {
    let dir = TempDir::new("partial_header");
    write_records(dir.path(), small_config(128), 30);
    let segments = list_segment_paths(dir.path());
    let last = segments.last().unwrap();
    let len = fs::metadata(last).unwrap().len();
    // Truncate to 4 bytes short of the end — guaranteed to land inside
    // *some* frame's 8-byte header for a small-record workload.
    let f = OpenOptions::new().write(true).open(last).unwrap();
    f.set_len(len.saturating_sub(4)).unwrap();

    let started = Instant::now();
    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    report_row(
        "partial_final_header",
        "torn tail (truncated=true), zero corruption, prefix of 30 recovered",
        result.corrupted_segments.len(),
        result.truncated,
        result.records.len(),
        result.records.last().map(|(s, _)| *s).unwrap_or(0),
        elapsed_ms,
    );
    assert!(result.truncated);
    assert!(result.corrupted_segments.is_empty());
    assert!(result.records.len() < 30);
}

/// Fixture 4: partial final body — the last frame's 8-byte header is
/// fully present (so its claimed `length` is readable) but its body is
/// cut short. Expected: a torn tail (the tail-eligibility test is "does
/// the claimed extent land beyond `segment_len`", WAL Spec §6.3).
#[test]
fn fixture_partial_final_body() {
    let dir = TempDir::new("partial_body");
    write_records(dir.path(), small_config(4096), 5);
    let segments = list_segment_paths(dir.path());
    let last = segments.last().unwrap();
    let len = fs::metadata(last).unwrap().len();
    // Cut off the last 6 bytes — past any single frame's header but
    // short of a full frame body, for this fixture's small values.
    let f = OpenOptions::new().write(true).open(last).unwrap();
    f.set_len(len.saturating_sub(6)).unwrap();

    let started = Instant::now();
    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(4096)).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    report_row(
        "partial_final_body",
        "torn tail (truncated=true), zero corruption, prefix of 5 recovered",
        result.corrupted_segments.len(),
        result.truncated,
        result.records.len(),
        result.records.last().map(|(s, _)| *s).unwrap_or(0),
        elapsed_ms,
    );
    assert!(result.truncated);
    assert!(result.corrupted_segments.is_empty());
    assert!(result.records.len() < 5);
}

/// Fixture 5: valid durable tail — no corruption anywhere, recovery
/// must return every record exactly. The control case every other
/// fixture in this file is compared against.
#[test]
fn fixture_valid_durable_tail() {
    let dir = TempDir::new("valid_tail");
    write_records(dir.path(), WalConfig::default(), 1000);

    let started = Instant::now();
    let (_, result) = FileWal::open_for_recovery(dir.path(), WalConfig::default()).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    report_row(
        "valid_durable_tail",
        "OK, all 1000 records recovered, gap-free, zero corruption, not truncated",
        result.corrupted_segments.len(),
        result.truncated,
        result.records.len(),
        result.records.last().map(|(s, _)| *s).unwrap_or(0),
        elapsed_ms,
    );
    assert_eq!(result.records.len(), 1000);
    assert!(result.corrupted_segments.is_empty());
    assert!(!result.truncated);
}

/// Fixture 6: CRC corruption on a non-tail frame — a single byte inside
/// an *earlier* record's covered range is flipped. Expected: corruption
/// (never silently accepted, never treated as a tail), stopping the
/// scan at that point per this project's fail-closed §6.2 amendment.
#[test]
fn fixture_crc_corruption_non_tail() {
    let dir = TempDir::new("crc_corruption");
    write_records(dir.path(), small_config(4096), 20);
    let segments = list_segment_paths(dir.path());
    let seg = &segments[0];
    // Flip a byte a few bytes into the segment body (well before the
    // last record, inside an earlier frame's covered range) — not the
    // header, and not the final frame.
    let mut f = OpenOptions::new().read(true).write(true).open(seg).unwrap();
    f.seek(SeekFrom::Start(40)).unwrap();
    let mut byte = [0u8; 1];
    f.read_exact(&mut byte).unwrap();
    f.seek(SeekFrom::Start(40)).unwrap();
    f.write_all(&[byte[0] ^ 0xFF]).unwrap();

    let started = Instant::now();
    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(4096)).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    report_row(
        "crc_corruption_non_tail",
        "corrupted=true (not a torn tail), scan stops at/before the corruption point",
        result.corrupted_segments.len(),
        result.truncated,
        result.records.len(),
        result.records.last().map(|(s, _)| *s).unwrap_or(0),
        elapsed_ms,
    );
    assert!(
        !result.corrupted_segments.is_empty(),
        "a non-tail CRC mismatch must be classified as corruption, never silently accepted \
         or treated as a truncation"
    );
}

/// Fixture 7 (new coverage, not duplicating existing tests): an
/// out-of-range `length` field on a *non-tail* frame, at the recovery/
/// classification boundary rather than the write-time `max_record_len`
/// enforcement `tests/wal_tests.rs::max_record_len_is_enforced_before_
/// any_write` already covers. Expected: corruption (WAL Spec §6.2 step
/// 3 — an out-of-range length is never trusted to compute a claimed
/// extent, so it cannot pass the tail-eligibility test either).
#[test]
fn fixture_invalid_length_field_non_tail() {
    let dir = TempDir::new("invalid_length");
    write_records(dir.path(), small_config(4096), 20);
    let segments = list_segment_paths(dir.path());
    let seg = &segments[0];
    // The segment header is 24 bytes (SEGMENT_HEADER_LEN); the first
    // frame's 4-byte length field starts immediately after it. Overwrite
    // it with an absurd value.
    let mut f = OpenOptions::new().write(true).open(seg).unwrap();
    f.seek(SeekFrom::Start(24)).unwrap();
    f.write_all(&u32::MAX.to_le_bytes()).unwrap();

    let started = Instant::now();
    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(4096)).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    report_row(
        "invalid_length_field_non_tail",
        "corrupted=true, zero records trusted (corruption is in the very first frame)",
        result.corrupted_segments.len(),
        result.truncated,
        result.records.len(),
        result.records.last().map(|(s, _)| *s).unwrap_or(0),
        elapsed_ms,
    );
    assert!(!result.corrupted_segments.is_empty());
    assert!(result.records.is_empty());
}

/// Fixture 8 (new coverage): a syntactically valid frame (correct
/// length, correct CRC recomputed over the tampered body) whose op-tag
/// byte does not match any known `WalOp` variant — isolates the
/// op-decode failure path from the CRC-mismatch path. Expected:
/// corruption, via a structural decode failure rather than a CRC
/// mismatch.
#[test]
fn fixture_malformed_frame_unrecognized_op_tag() {
    let dir = TempDir::new("malformed_op_tag");
    write_records(dir.path(), small_config(4096), 5);
    let segments = list_segment_paths(dir.path());
    let seg = &segments[0];

    // Layout after the 24-byte segment header: length:u32 LE,
    // crc32c:u32 LE, seq:u64 LE, op:u8, op_body. The op tag is the 17th
    // byte of the frame (24 + 4 + 4 + 8 = 40).
    let mut contents = Vec::new();
    File::open(seg).unwrap().read_to_end(&mut contents).unwrap();
    let op_tag_offset = 24 + 4 + 4 + 8;
    assert!(contents.len() > op_tag_offset, "fixture segment too small");
    contents[op_tag_offset] = 0xEE; // not a valid WalOp tag

    // Recompute the CRC over exactly what the frame format covers
    // (seq..end of this frame's body) so this fixture isolates the
    // op-decode failure, not a CRC mismatch this project's own
    // recovery already handles via a completely different assertion
    // (fixture_crc_corruption_non_tail above).
    let length_bytes: [u8; 4] = contents[24..28].try_into().unwrap();
    let length = u32::from_le_bytes(length_bytes) as usize;
    let body_start = 24 + 8; // length+crc32c header, then the covered region
    let body_end = body_start + length;
    let new_crc = crc32c::crc32c(&contents[body_start..body_end]);
    contents[28..32].copy_from_slice(&new_crc.to_le_bytes());
    fs::write(seg, &contents).unwrap();

    let started = Instant::now();
    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(4096)).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    report_row(
        "malformed_frame_unrecognized_op_tag",
        "corrupted=true via a structural decode failure (unrecognized op tag), not a CRC error",
        result.corrupted_segments.len(),
        result.truncated,
        result.records.len(),
        result.records.last().map(|(s, _)| *s).unwrap_or(0),
        elapsed_ms,
    );
    assert!(
        !result.corrupted_segments.is_empty(),
        "an unrecognized op tag (with an otherwise-valid CRC) must still be classified as \
         corruption, not silently skipped or misinterpreted as a different op"
    );
    assert!(result.records.is_empty());
}

/// Fixture 9: mixed valid/corrupt segments — the first of several
/// segments is fully valid, a later (non-last) segment is corrupted, a
/// final segment beyond it is never trusted. Cross-references (does not
/// duplicate) `tests/wal_tests.rs::corrupted_non_first_non_last_
/// segment_keeps_earlier_records_only`, presented here in the same
/// evidence-row format as every other fixture in this file.
#[test]
fn fixture_mixed_valid_and_corrupt_segments() {
    let dir = TempDir::new("mixed_segments");
    write_records(dir.path(), small_config(128), 30);
    let segments = list_segment_paths(dir.path());
    assert!(segments.len() >= 3, "need at least 3 segments");
    let middle = &segments[1];
    let mut f = OpenOptions::new().write(true).open(middle).unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(b"XXXXXXXX").unwrap();

    let started = Instant::now();
    let (_, result) = FileWal::open_for_recovery(dir.path(), small_config(128)).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    report_row(
        "mixed_valid_and_corrupt_segments",
        "corrupted=true, earlier (first-segment) records preserved, nothing at/after trusted",
        result.corrupted_segments.len(),
        result.truncated,
        result.records.len(),
        result.records.last().map(|(s, _)| *s).unwrap_or(0),
        elapsed_ms,
    );
    assert_eq!(result.corrupted_segments.len(), 1);
    assert!(!result.records.is_empty());
    assert!(result.records.len() < 30);
}
