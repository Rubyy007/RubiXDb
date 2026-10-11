//! Scratch fixture generator (NOT part of the repository): writes N large SSTables through the real engine.
//! usage: f08gen <data_dir> <tables> <table_mib> <value_len> <threads>
use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&a[1]);
    let tables: usize = a[2].parse().unwrap();
    let table_mib: usize = a[3].parse().unwrap();
    let vlen: usize = a[4].parse().unwrap();
    let threads: usize = a[5].parse().unwrap();
    let engine = Arc::new(
        LsmEngine::open(
            &dir,
            WalConfig {
                sync_mode: SyncMode::GroupCommit { max_wait: Duration::from_millis(5), max_batch_bytes: 256 * 1024 },
                ..WalConfig::default()
            },
            BatchCoordinatorConfig {
                queue_capacity: 4096,
                max_queued_bytes: 64 * 1024 * 1024,
                submission_timeout: Duration::from_secs(60),
                shutdown_drain_bound: Duration::from_secs(60),
                await_retry_budget: Duration::from_secs(60),
                max_drain_per_batch: 65536,
            },
            LsmConfig {
                memtable_max_size_bytes: table_mib * 1024 * 1024,
                compaction_auto_trigger: false,
                ..LsmConfig::default()
            },
        )
        .unwrap(),
    );
    let per_table = (table_mib * 1024 * 1024) / (vlen + 48);
    let mut total = (per_table as u64) * (tables as u64) + (per_table as u64) / 4; // a quarter table extra so the last memtable freezes
    if a.len() > 6 { total = a[6].parse().unwrap(); }
    let next = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();
    let mut hs = vec![];
    for _ in 0..threads {
        let (e, n) = (engine.clone(), next.clone());
        hs.push(std::thread::spawn(move || loop {
            let i = n.fetch_add(1, Ordering::SeqCst);
            if i >= total {
                break;
            }
            let mut v = vec![0u8; vlen];
            // deterministic, non-constant content per record
            let mut x = i.wrapping_mul(0x9E3779B97F4A7C15) | 1;
            for c in v.chunks_mut(8) {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                let b = x.to_le_bytes();
                c.copy_from_slice(&b[..c.len()]);
            }
            e.put(format!("blob-{i:010}").as_bytes(), &v).unwrap();
        }));
    }
    for h in hs { h.join().unwrap(); }
    eprintln!("puts done: {total} in {:?}", t0.elapsed());
    let deadline = Instant::now() + Duration::from_secs(900);
    while engine.sstable_count() < tables && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!("sstables: {}", engine.sstable_count());
    for i in 0..20u32 { engine.put(format!("tail-{i:03}").as_bytes(), b"t").unwrap(); }
    engine.shutdown();
    drop(engine);
    eprintln!("done in {:?}", t0.elapsed());
}
