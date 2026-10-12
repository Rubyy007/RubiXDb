//! F-08 / GAP-B: the footer `record_count` bound enforced by `SsTable::open` (constant time, before the bloom block is read).
//!
//! Every table is written by the real writer into a disposable temp directory and then has only its footer `record_count`
//! rewritten with a valid footer CRC (`Footer::encode` recomputes it), the shape of the F-08 verification fixture
//! (`scratch/f08_verification/scripts/recordcount_consumer.py`). A counting global allocator records the largest single
//! allocation made while `SsTable::open` runs, to prove that a rejected table never reaches a bloom-sized allocation.
//! All tests take one lock so the allocator's measurement window is never shared with another test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use rubixdb::error::EngineError;
use rubixdb::sstable::format::{Footer, FOOTER_SIZE};
use rubixdb::sstable::{write_from_sorted_records, RecordValue, SsTable, SsTableWriterConfig};

struct Counting;
static TRACK: AtomicBool = AtomicBool::new(false);
static LARGEST: AtomicUsize = AtomicUsize::new(0);

fn note(size: usize) {
    if TRACK.load(Ordering::SeqCst) {
        LARGEST.fetch_max(size, Ordering::SeqCst);
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note(l.size());
        System.alloc(l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        note(l.size());
        System.alloc_zeroed(l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        System.realloc(p, l, new_size)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static A: Counting = Counting;

static SERIAL: Mutex<()> = Mutex::new(());

struct Dir(PathBuf);
impl Dir {
    fn new(tag: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("rubixdb_f08_rcb_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A real table of `n` records (`key-000000..`, 100-byte values), written by the real writer.
fn write_table(dir: &Path, n: u32) -> PathBuf {
    let records = (0..n).map(|i| {
        Ok((
            format!("key-{i:06}").into_bytes(),
            u64::from(i) + 1,
            RecordValue::Put(vec![0x5A; 100]),
        ))
    });
    let meta = write_from_sorted_records(
        records,
        1,
        dir,
        &SsTableWriterConfig::default(),
        u64::from(n),
    )
    .unwrap();
    assert_eq!(meta.record_count, u64::from(n));
    meta.path
}

/// Rewrites only the footer `record_count`, recomputing the footer CRC.
fn set_record_count(path: &Path, count: u64) {
    let mut b = std::fs::read(path).unwrap();
    let at = b.len() - FOOTER_SIZE;
    let mut f = Footer::decode(&b[at..]).unwrap();
    f.record_count = count;
    b[at..].copy_from_slice(&f.encode());
    std::fs::write(path, b).unwrap();
}

/// Opens `path` and returns the result with the largest single allocation made during the call.
fn open_measured(path: &Path) -> (Result<SsTable, EngineError>, usize) {
    LARGEST.store(0, Ordering::SeqCst);
    TRACK.store(true, Ordering::SeqCst);
    let r = SsTable::open(path, 1);
    TRACK.store(false, Ordering::SeqCst);
    (r, LARGEST.load(Ordering::SeqCst))
}

fn corruption_detail(r: Result<SsTable, EngineError>) -> String {
    match r {
        Err(EngineError::Corruption { detail }) => detail,
        Ok(_) => panic!("expected the open to be refused, it succeeded"),
        Err(other) => panic!("expected EngineError::Corruption, got {other:?}"),
    }
}

#[test]
fn a_normal_table_opens_reads_and_keeps_its_count() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let d = Dir::new("normal");
    let path = write_table(&d.0, 500);
    let (t, _) = open_measured(&path);
    let t = t.expect("a valid table must open");
    assert_eq!(t.record_count(), 500);
    let (seq, v) = t
        .get_versioned(b"key-000123", u64::MAX)
        .unwrap()
        .expect("the record is readable");
    assert_eq!(seq, 124);
    assert!(matches!(v, RecordValue::Put(ref b) if b.len() == 100));
}

#[test]
fn the_bound_is_accepted_and_the_next_integer_is_rejected() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let d = Dir::new("boundary");
    let path = write_table(&d.0, 50);
    // Independent expectation (floor(8 * 268_435_456 / 10)); the unit tests in `bloom.rs` derive it from the sizing formula.
    let bound: u64 = 214_748_364;
    // The footer lies, but exactly at the bound the bloom a writer would size from it still fits: not refused here.
    // (The mismatch with the 50 stored records is NOT detected by this bound; that is the documented limitation.)
    set_record_count(&path, bound);
    let t = SsTable::open(&path, 1).expect("the bound itself is accepted");
    assert_eq!(t.record_count(), bound);
    drop(t);
    // One more is refused.
    set_record_count(&path, bound + 1);
    let detail = corruption_detail(SsTable::open(&path, 1));
    assert!(detail.contains("record_count"), "{detail}");
    assert!(detail.contains("214748364"), "{detail}");
    // The extreme does not overflow or panic.
    set_record_count(&path, u64::MAX);
    let detail = corruption_detail(SsTable::open(&path, 1));
    assert!(detail.contains("record_count"), "{detail}");
}

#[test]
fn four_hundred_million_with_a_valid_footer_crc_is_rejected_before_any_bloom_sized_allocation() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let d = Dir::new("fixture");
    let path = write_table(&d.0, 500);
    // Control: the honest table's largest allocation during open (bloom, index, buffers) is small.
    let (ok, honest_largest) = open_measured(&path);
    ok.unwrap();
    assert!(honest_largest < 1024 * 1024, "control: {honest_largest}");

    set_record_count(&path, 400_000_000);
    // The footer is still self-consistent: the CRC was recomputed, so only the new check can refuse it.
    let raw = std::fs::read(&path).unwrap();
    Footer::decode(&raw[raw.len() - FOOTER_SIZE..]).expect("the rewritten footer has a valid CRC");
    let before = std::fs::read(&path).unwrap();

    let (r, largest) = open_measured(&path);
    let detail = corruption_detail(r);
    assert!(detail.contains("record_count"), "{detail}");
    assert!(detail.contains("400000000"), "{detail}");
    // 400,000,000 records at 10 bits per key would be a 500,000,000 byte bloom. Nothing near that was allocated.
    assert!(
        largest < 1024 * 1024,
        "largest single allocation during the refused open was {largest} bytes"
    );
    assert!(largest <= honest_largest.max(64 * 1024));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "open must not modify the file"
    );
}

#[test]
fn the_check_runs_before_the_bloom_block_is_read_and_without_touching_data_blocks() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let d = Dir::new("ordering");
    let path = write_table(&d.0, 500);
    let footer_at = std::fs::metadata(&path).unwrap().len() as usize - FOOTER_SIZE;
    let f = Footer::decode(&std::fs::read(&path).unwrap()[footer_at..]).unwrap();

    // Control: a damaged bloom with an honest count is reported as the bloom failure.
    let mut b = std::fs::read(&path).unwrap();
    b[f.bloom_offset as usize + 9 + 3] ^= 0xFF;
    std::fs::write(&path, &b).unwrap();
    assert!(corruption_detail(SsTable::open(&path, 1)).contains("bloom: checksum mismatch"));

    // With an inflated count as well, the count is refused first: the bloom block was never read or decoded.
    set_record_count(&path, 400_000_000);
    assert!(corruption_detail(SsTable::open(&path, 1)).contains("record_count"));
}

#[test]
fn open_reads_no_data_block_and_the_refusal_is_the_same_with_wrecked_data_blocks() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let d = Dir::new("datablocks");
    let path = write_table(&d.0, 500);
    let footer_at = std::fs::metadata(&path).unwrap().len() as usize - FOOTER_SIZE;
    let f = Footer::decode(&std::fs::read(&path).unwrap()[footer_at..]).unwrap();
    // Wreck every data-block byte (everything before the bloom block): open still succeeds, so it reads none of them.
    let mut b = std::fs::read(&path).unwrap();
    for x in &mut b[..f.bloom_offset as usize] {
        *x ^= 0xFF;
    }
    std::fs::write(&path, &b).unwrap();
    SsTable::open(&path, 1).expect("open reads no data block");
    // The inflated count is refused identically.
    set_record_count(&path, 400_000_000);
    assert!(corruption_detail(SsTable::open(&path, 1)).contains("record_count"));
}

#[test]
fn a_table_the_bound_refuses_is_also_refused_by_the_product_preflight_path_as_the_engine_text() {
    // The preflight calls `SsTable::open` and maps its error to SSTABLE_CORRUPT carrying the engine text (src/ops tests
    // cover the mapping end to end); here: the engine text is the typed Corruption classification, not a generic error.
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let d = Dir::new("typed");
    let path = write_table(&d.0, 20);
    set_record_count(&path, 400_000_000);
    match SsTable::open(&path, 1) {
        Err(EngineError::Corruption { detail }) => assert!(!detail.is_empty()),
        other => panic!("expected typed Corruption, got {:?}", other.map(|_| ())),
    }
}
