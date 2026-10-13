//! Shared by `examples/fault_child.rs` (the process that gets killed) and `tests/fault_points/` (the parent that judges
//! the result) through `#[path]`, so both derive the *same* operations from the same seed. The parent never reads the
//! expected data back from the engine under test: it regenerates it here.
//!
//! The ledger is the only thing the child reports: which operations it submitted, which the engine acknowledged (with the
//! seq the engine returned) and which returned an error. It is appended with one `write_all` per line to a file outside
//! the data directory; a process kill cannot lose a line that `write_all` returned from (class B only - and this file is
//! test machinery, not evidence about the engine's durability).

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub seed: u64,
    pub writers: usize,
    /// Commit units per writer.
    pub ops: usize,
    /// Distinct keys per writer (small, so keys are overwritten and deleted).
    pub keys: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
}

/// One commit unit: a single put/delete, or a 3-item batch (atomic).
pub type Op = Vec<Item>;

pub fn key(writer: usize, k: usize) -> Vec<u8> {
    format!("w{writer:02}-k{k:03}").into_bytes()
}

pub fn writer_prefix(writer: usize) -> Vec<u8> {
    format!("w{writer:02}-").into_bytes()
}

fn value(p: &Params, w: usize, i: usize, item: usize, h: u64) -> Vec<u8> {
    let len = 90 + ((h >> 24) % 110) as usize;
    let mut v = format!("v{w}.{i}.{item}.").into_bytes();
    let mut x = h;
    while v.len() < len {
        x = splitmix64(x ^ p.seed);
        v.push(b'a' + (x % 26) as u8);
    }
    v
}

/// Commit unit `i` of writer `w`: a pure function of `(params, w, i)`.
pub fn gen_op(p: &Params, w: usize, i: usize) -> Op {
    let h = splitmix64(p.seed ^ ((w as u64) << 40) ^ (i as u64).wrapping_mul(0x1_0000_0001));
    let k0 = ((h >> 8) % p.keys as u64) as usize;
    match h % 100 {
        0..=69 => vec![Item::Put(key(w, k0), value(p, w, i, 0, h))],
        70..=84 => vec![Item::Del(key(w, k0))],
        _ => (0..3)
            .map(|j| {
                let k = (k0 + j) % p.keys;
                if (h >> (30 + j)) & 1 == 1 {
                    Item::Del(key(w, k))
                } else {
                    Item::Put(key(w, k), value(p, w, i, j, splitmix64(h ^ j as u64)))
                }
            })
            .collect(),
    }
}

pub fn apply(model: &mut BTreeMap<Vec<u8>, Vec<u8>>, op: &Op) {
    for it in op {
        match it {
            Item::Put(k, v) => {
                model.insert(k.clone(), v.clone());
            }
            Item::Del(k) => {
                model.remove(k);
            }
        }
    }
}

/// The state after the first `k` commit units of writer `w`.
pub fn model_prefix(p: &Params, w: usize, k: usize) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut m = BTreeMap::new();
    for i in 0..k {
        apply(&mut m, &gen_op(p, w, i));
    }
    m
}

// ---------------------------------------------------------------------------------------------------------------------
// ledger
// ---------------------------------------------------------------------------------------------------------------------

pub fn ledger_path(dir: &Path, w: usize) -> PathBuf {
    dir.join(format!("ledger_w{w}.txt"))
}

pub struct LedgerWriter {
    file: File,
}

impl LedgerWriter {
    pub fn create(dir: &Path, w: usize) -> LedgerWriter {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(ledger_path(dir, w))
            .expect("ledger file");
        LedgerWriter { file }
    }

    fn line(&mut self, s: String) {
        self.file.write_all(s.as_bytes()).expect("ledger write");
    }

    pub fn submit(&mut self, i: usize) {
        self.line(format!("S {i}\n"));
    }

    pub fn ack(&mut self, i: usize, seq: u64) {
        self.line(format!("A {i} {seq}\n"));
    }

    pub fn err(&mut self, i: usize, msg: &str) {
        self.line(format!("E {i} {}\n", msg.replace('\n', " ")));
    }
}

#[derive(Debug, Default, Clone)]
pub struct WriterLedger {
    /// Operations whose submission was recorded (a prefix `0..submitted`).
    pub submitted: usize,
    /// Operations the engine acknowledged (a prefix `0..acked`), with their acknowledged seq.
    pub acked: usize,
    pub ack_seqs: Vec<u64>,
    /// Operations that returned an error (outcome unknown).
    pub errored: usize,
}

pub fn read_ledger(dir: &Path, w: usize) -> Result<WriterLedger, String> {
    let text = match std::fs::read_to_string(ledger_path(dir, w)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WriterLedger::default()),
        Err(e) => return Err(format!("ledger unreadable: {e}")),
    };
    let mut l = WriterLedger::default();
    for (n, line) in text.lines().enumerate() {
        let f: Vec<&str> = line.splitn(3, ' ').collect();
        match f.as_slice() {
            ["S", i] => {
                let i: usize = i
                    .parse()
                    .map_err(|_| format!("ledger line {n}: bad index"))?;
                if i != l.submitted {
                    return Err(format!(
                        "ledger line {n}: submit {i} out of order (expected {})",
                        l.submitted
                    ));
                }
                l.submitted += 1;
            }
            ["A", i, seq] => {
                let i: usize = i
                    .parse()
                    .map_err(|_| format!("ledger line {n}: bad index"))?;
                if i != l.acked || i + 1 != l.submitted {
                    return Err(format!(
                        "ledger line {n}: ack {i} out of order (acked {}, submitted {})",
                        l.acked, l.submitted
                    ));
                }
                l.ack_seqs.push(
                    seq.parse()
                        .map_err(|_| format!("ledger line {n}: bad seq"))?,
                );
                l.acked += 1;
            }
            ["E", ..] => l.errored += 1,
            _ => {
                // A torn last line (the process died inside write_all) is conservative: it is ignored and the operation
                // stays "submitted, outcome unknown".
                if n + 1 != text.lines().count() {
                    return Err(format!("ledger line {n} unparsable: {line:?}"));
                }
            }
        }
    }
    Ok(l)
}

// ---------------------------------------------------------------------------------------------------------------------
// engine configuration used by the workload child (small structures so flush, rotation and compaction happen)
// ---------------------------------------------------------------------------------------------------------------------

pub mod cfg {
    use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
    use rubixdb::lsm::LsmConfig;
    use rubixdb::wal::{SyncMode, WalConfig};
    use std::time::Duration;

    /// Product sync mode (GroupCommit 5 ms / 256 KiB) with a small segment so the WAL rotates and is purged.
    pub fn wal() -> WalConfig {
        WalConfig {
            sync_mode: SyncMode::GroupCommit {
                max_wait: Duration::from_millis(5),
                max_batch_bytes: 256 * 1024,
            },
            max_segment_size: 24 * 1024,
            ..WalConfig::default()
        }
    }

    pub fn pool() -> BatchCoordinatorConfig {
        BatchCoordinatorConfig {
            queue_capacity: 512,
            max_queued_bytes: 16 * 1024 * 1024,
            submission_timeout: Duration::from_secs(5),
            shutdown_drain_bound: Duration::from_secs(10),
            await_retry_budget: Duration::from_secs(5),
            max_drain_per_batch: 4096,
        }
    }

    /// A small MemTable (frequent flush) and a low compaction trigger with the automatic worker on.
    pub fn lsm() -> LsmConfig {
        LsmConfig {
            memtable_max_size_bytes: 8 * 1024,
            compaction_auto_trigger: true,
            compaction_trigger_count: 2,
            ..LsmConfig::default()
        }
    }
}
