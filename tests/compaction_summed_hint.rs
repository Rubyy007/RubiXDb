//! F-08 / GAP-B follow-up: compaction sums its inputs' footer `record_count` into the output bloom's size hint. Each input is
//! individually within the per-table bound `SsTable::open` enforces, yet the SUM can exceed the largest count whose bloom the
//! reader accepts. Real engine, real background worker, disposable temp directories; only the inputs' footer `record_count`
//! is rewritten (footer CRC recomputed by `Footer::encode`), the shape of the F-08 verification fixture.
//!
//! Cases: a normal compaction (unchanged), the summed hint exactly at the cap, and one above it. Each opens the output, reads
//! every key back, and checks the worker's metrics and the directory.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::sstable::format::{Footer, FOOTER_SIZE};
use rubixdb::sstable::SsTable;
use rubixdb::wal::{SyncMode, WalConfig};

const CAP: u64 = 214_748_364;
static SERIAL: Mutex<()> = Mutex::new(());

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

fn lsm_cfg(auto: bool) -> LsmConfig {
    LsmConfig {
        memtable_max_size_bytes: 48 * 1024,
        compaction_auto_trigger: auto,
        compaction_trigger_count: 4,
        ..LsmConfig::default()
    }
}

/// Same, with a very large `bloom_bits_per_key`: the per-table cap is `floor(2^31 / bits_per_key)` records, so 3,000,000 bits
/// per key scales the cap down to 715 records and lets the 1,428 survivors of the fixture exceed it (the honest-scale case of
/// more than 214,748,364 survivors, in miniature).
fn lsm_cfg_bits(bits: u32) -> LsmConfig {
    LsmConfig {
        bloom_bits_per_key: bits,
        ..lsm_cfg(true)
    }
}

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ssts(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir.join("sstables"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sst"))
        .collect();
    v.sort();
    v
}

fn footer_count(path: &Path) -> u64 {
    let b = std::fs::read(path).unwrap();
    Footer::decode(&b[b.len() - FOOTER_SIZE..])
        .unwrap()
        .record_count
}

fn set_footer_count(path: &Path, count: u64) {
    let mut b = std::fs::read(path).unwrap();
    let at = b.len() - FOOTER_SIZE;
    let mut f = Footer::decode(&b[at..]).unwrap();
    f.record_count = count;
    b[at..].copy_from_slice(&f.encode());
    std::fs::write(path, b).unwrap();
}

/// Writes exactly 1,500 keys through the real engine (auto compaction OFF), leaving at least four tables; stopped cleanly.
fn build(tag: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("rubixdb_f08_sum_{}_{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let e = LsmEngine::open(&d, wal_cfg(), pool_cfg(), lsm_cfg(false)).unwrap();
    let v = vec![0x5Au8; 200];
    for i in 0..1500u32 {
        e.put(format!("key-{i:06}").as_bytes(), &v).unwrap();
    }
    let t = Instant::now();
    while e.sstable_count() < 4 && t.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(e.sstable_count() >= 4, "the fixture needs four live tables");
    e.shutdown();
    drop(e);
    Dir(d)
}

struct Outcome {
    cycles: u64,
    failures: u64,
    blocked: bool,
    live_after: usize,
    keys_ok: bool,
}

/// Reopens with the background worker ON and waits for the first cycle or the first failure.
fn compact(d: &Dir) -> Outcome {
    compact_with(d, lsm_cfg(true), false)
}

/// `until_blocked`: keep waiting until the worker blocks (instead of stopping at the first cycle or failure).
fn compact_with(d: &Dir, cfg: LsmConfig, until_blocked: bool) -> Outcome {
    let e = LsmEngine::open(&d.0, wal_cfg(), pool_cfg(), cfg).unwrap();
    let t = Instant::now();
    loop {
        let m = e.compaction_metrics();
        let done = if until_blocked {
            m.blocked
        } else {
            m.cycles_completed >= 1 || m.failures_total >= 1
        };
        if done || t.elapsed() > Duration::from_secs(150) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let m = e.compaction_metrics();
    let keys_ok = (0..1500u32).all(|i| {
        e.get(format!("key-{i:06}").as_bytes())
            .map(|v| v.is_some_and(|b| b.len() == 200))
            .unwrap_or(false)
    });
    let o = Outcome {
        cycles: m.cycles_completed,
        failures: m.failures_total,
        blocked: m.blocked,
        live_after: e.sstable_count(),
        keys_ok,
    };
    e.shutdown();
    drop(e);
    o
}

/// Every published `.sst` in the directory opens (an unreadable committed output would not).
fn all_tables_open(dir: &Path) -> bool {
    ssts(dir)
        .iter()
        .enumerate()
        .all(|(i, p)| SsTable::open(p, i as u64 + 1).is_ok())
}

#[test]
fn a_normal_compaction_is_unaffected() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let d = build("normal");
    let inputs = ssts(&d.0);
    let total: u64 = inputs.iter().map(|p| footer_count(p)).sum();
    assert!(total > 0 && total < 100_000);
    let o = compact(&d);
    assert_eq!(o.cycles, 1);
    assert_eq!(o.failures, 0);
    assert!(!o.blocked && o.keys_ok);
    // one live output; its bloom was sized from the plain sum of the inputs' counts: 8 + 1 + ceil(total * 10 / 8) + 4 bytes
    let live = ssts(&d.0);
    let out = live
        .iter()
        .max_by_key(|p| std::fs::metadata(p).unwrap().len())
        .unwrap();
    let b = std::fs::read(out).unwrap();
    let f = Footer::decode(&b[b.len() - FOOTER_SIZE..]).unwrap();
    // every key was written once, so nothing is dropped: the output holds exactly the inputs' records
    assert_eq!(f.record_count, total);
    assert_eq!(f.bloom_length, 13 + (total * 10).div_ceil(8));
    assert!(all_tables_open(&d.0));
}

/// Sets table 1's footer count so that the inputs' sum is exactly `sum`.
fn tamper_to_sum(d: &Dir, sum: u64) {
    let inputs = ssts(&d.0);
    let others: u64 = inputs.iter().skip(1).map(|p| footer_count(p)).sum();
    assert!(sum > others);
    set_footer_count(&inputs[0], sum - others);
}

#[test]
fn a_summed_hint_exactly_at_the_cap_compacts_to_one_readable_output() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let d = build("at_cap");
    tamper_to_sum(&d, CAP);
    let o = compact(&d);
    assert_eq!(o.failures, 0, "no failed attempt");
    assert_eq!(o.cycles, 1, "the cycle completed");
    assert!(!o.blocked);
    assert!(o.keys_ok, "every key reads back");
    assert!(all_tables_open(&d.0), "every published table opens");
    assert_eq!(o.live_after, 1);
}

#[test]
fn a_summed_hint_one_above_the_cap_is_clamped_and_compacts_to_one_readable_output() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let d = build("over_cap");
    tamper_to_sum(&d, CAP + 1);
    let o = compact(&d);
    assert_eq!(o.failures, 0, "no failed attempt");
    assert_eq!(o.cycles, 1, "the cycle completed");
    assert!(!o.blocked);
    assert!(o.keys_ok, "every key reads back");
    assert!(all_tables_open(&d.0), "every published table opens");
    assert_eq!(o.live_after, 1);
}

#[test]
fn an_output_above_the_cap_is_refused_before_the_manifest_commit_and_the_worker_blocks() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let d = build("guard");
    let before: Vec<String> = ssts(&d.0)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    let manifest_before = std::fs::read(d.0.join("MANIFEST")).unwrap();
    let o = compact_with(&d, lsm_cfg_bits(3_000_000), true);
    assert_eq!(o.cycles, 0, "no cycle may complete");
    assert!(o.failures >= 1, "the attempts are failures");
    assert!(
        o.blocked,
        "a permanent-candidate failure blocks the worker after its budget"
    );
    assert!(
        o.keys_ok,
        "every key is still readable from the untouched inputs"
    );
    let after: Vec<String> = ssts(&d.0)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(after, before, "no output table survived a refused attempt");
    assert!(all_tables_open(&d.0));
    assert_eq!(
        std::fs::read(d.0.join("MANIFEST")).unwrap(),
        manifest_before,
        "nothing was committed to the Manifest"
    );
    assert_eq!(o.live_after, before.len());
}
